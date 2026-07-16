//! Raw-signal bypass grep guard.
//!
//! This test loads the critical source files at compile time and verifies that
//! classifier integration patterns are present. If a refactor drops the
//! classifier gate from a consumer, this test fails — preventing merge of
//! code that bypasses the liveness classifier and independently decides from
//! raw pod phase, in-memory activity, or DB state alone.
//!
//! Companion runbook: `server/docs/operational/raw-signal-bypass-audit.md`

/// Source of the primary session recovery consumers (stall, zombie, idle, kill).
const SESSION_RECOVERY_SRC: &str = include_str!("../dispatch/session_recovery.rs");

// ── Session recovery classifier integration guards ────────────────────────

/// The stall-timeout consumer must call `classify_task_liveness` to consult
/// the liveness classifier before terminal transitions.
#[test]
fn stall_recovery_calls_classifier() {
    assert!(
        SESSION_RECOVERY_SRC.contains("classify_task_liveness"),
        "session_recovery.rs must call classify_task_liveness for stall recovery"
    );
}

/// The zombie-running reaper must call `classify_task_liveness` and gate
/// reap on the verdict — a `Live` or `Slow` verdict must suppress reap.
#[test]
fn zombie_reap_gates_on_classifier_verdict() {
    assert!(
        SESSION_RECOVERY_SRC.contains("Verdict::Live")
            || SESSION_RECOVERY_SRC.contains("Verdict::Slow"),
        "session_recovery.rs zombie reap must gate on Verdict::Live/Verdict::Slow"
    );
    assert!(
        SESSION_RECOVERY_SRC.contains("LivenessOutcome::KillNoop"),
        "session_recovery.rs zombie reap must check LivenessOutcome::KillNoop for terminal-task races"
    );
}

/// Every kill path must persist a `LivenessEvidenceSnapshot` before
/// terminal transitions — evidence must flow to the liveness_evidence table
/// for the board_health and doctor diagnostics surfaces.
#[test]
fn kill_paths_persist_liveness_evidence() {
    assert!(
        SESSION_RECOVERY_SRC.contains("LivenessEvidenceSnapshot"),
        "session_recovery.rs must persist LivenessEvidenceSnapshot for kill paths"
    );
    assert!(
        SESSION_RECOVERY_SRC.contains("LivenessRepository"),
        "session_recovery.rs must use LivenessRepository for evidence persistence"
    );
}

/// The `ClassificationResult` type must be used — raw signals alone must not
/// drive decisions without passing through the classifier.
#[test]
fn session_recovery_uses_classifier_result() {
    assert!(
        SESSION_RECOVERY_SRC.contains("ClassificationResult"),
        "session_recovery.rs must use ClassificationResult from the classifier"
    );
}

/// The `classify_session_exit_liveness` path must exist for protocol-violation
/// detection on session exit (clean exit on nonterminal task, etc.).
#[test]
fn session_exit_liveness_classification_exists() {
    assert!(
        SESSION_RECOVERY_SRC.contains("classify_session_exit_liveness"),
        "session_recovery.rs must have classify_session_exit_liveness for exit-time protocol violation detection"
    );
}

/// The `Verdict` type must be imported — showing the code uses the verdict
/// enum rather than raw string comparisons on DB fields.
#[test]
fn session_recovery_imports_verdict_enum() {
    assert!(
        SESSION_RECOVERY_SRC.contains("super::liveness::")
            && SESSION_RECOVERY_SRC.contains("Verdict"),
        "session_recovery.rs must import Verdict from the liveness module"
    );
}
