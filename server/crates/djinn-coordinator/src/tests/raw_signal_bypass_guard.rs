//! Raw-signal bypass grep guard.
//!
//! This test loads the critical source files at compile time and verifies that
//! classifier integration patterns are present. If a refactor drops the
//! classifier gate from a consumer, this test fails — preventing merge of
//! code that bypasses the liveness classifier and independently decides from
//! raw pod phase, in-memory activity, or DB state alone.
//!
//! Every assertion here matches against PRODUCTION code only: comment bodies,
//! literal bodies, and `#[cfg(test)]` items are removed first (see
//! `boundary::production_code`). Matching the raw file could not do
//! the job the module header claims. `classify_task_liveness` appears in nine
//! comments and six `tracing` message literals in the guarded file, and
//! `Verdict::Live` in another comment, so the entire classifier gate could be
//! deleted from the stall path with every assertion below still green on the
//! surviving prose. That is the `1j64` false-negative shape, and it was live
//! here.
//!
//! Companion runbook: `server/docs/operational/raw-signal-bypass-audit.md`

use super::boundary::production_code;

/// Source of the primary session recovery consumers (stall, zombie, idle, kill).
const SESSION_RECOVERY_SRC: &str = include_str!("../dispatch/session_recovery.rs");

/// `SESSION_RECOVERY_SRC` reduced to production code.
///
/// A scanner failure fails the guard rather than reporting a clean file: an
/// unscannable source is not evidence that the classifier gate is present.
fn session_recovery_code() -> String {
    production_code(SESSION_RECOVERY_SRC)
        .unwrap_or_else(|error| panic!("session_recovery.rs must be scannable: {error}"))
}

/// Whether `needle` appears in the production code of `source`.
///
/// Extracted so the guard's own logic is testable against synthetic input —
/// otherwise "ignores comments" and "ignores everything" are indistinguishable
/// from a green run.
fn production_mentions(source: &str, needle: &str) -> bool {
    production_code(source)
        .expect("the fixture is scannable")
        .contains(needle)
}

// ── Session recovery classifier integration guards ────────────────────────

/// The stall-timeout consumer must call `classify_task_liveness` to consult
/// the liveness classifier before terminal transitions.
#[test]
fn stall_recovery_calls_classifier() {
    assert!(
        session_recovery_code().contains("classify_task_liveness"),
        "session_recovery.rs must call classify_task_liveness for stall recovery"
    );
}

/// The zombie-running reaper must call `classify_task_liveness` and gate
/// reap on the verdict — a `Live` or `Slow` verdict must suppress reap.
#[test]
fn zombie_reap_gates_on_classifier_verdict() {
    let code = session_recovery_code();
    assert!(
        code.contains("Verdict::Live") || code.contains("Verdict::Slow"),
        "session_recovery.rs zombie reap must gate on Verdict::Live/Verdict::Slow"
    );
    assert!(
        code.contains("LivenessOutcome::KillNoop"),
        "session_recovery.rs zombie reap must check LivenessOutcome::KillNoop for terminal-task races"
    );
}

/// Every kill path must persist a `LivenessEvidenceSnapshot` before
/// terminal transitions — evidence must flow to the liveness_evidence table
/// for the board_health and doctor diagnostics surfaces.
#[test]
fn kill_paths_persist_liveness_evidence() {
    let code = session_recovery_code();
    assert!(
        code.contains("LivenessEvidenceSnapshot"),
        "session_recovery.rs must persist LivenessEvidenceSnapshot for kill paths"
    );
    assert!(
        code.contains("LivenessRepository"),
        "session_recovery.rs must use LivenessRepository for evidence persistence"
    );
}

/// The `ClassificationResult` type must be used — raw signals alone must not
/// drive decisions without passing through the classifier.
#[test]
fn session_recovery_uses_classifier_result() {
    assert!(
        session_recovery_code().contains("ClassificationResult"),
        "session_recovery.rs must use ClassificationResult from the classifier"
    );
}

/// The `classify_session_exit_liveness` path must exist for protocol-violation
/// detection on session exit (clean exit on nonterminal task, etc.).
#[test]
fn session_exit_liveness_classification_exists() {
    assert!(
        session_recovery_code().contains("classify_session_exit_liveness"),
        "session_recovery.rs must have classify_session_exit_liveness for exit-time protocol violation detection"
    );
}

/// The `Verdict` type must be imported — showing the code uses the verdict
/// enum rather than raw string comparisons on DB fields.
#[test]
fn session_recovery_imports_verdict_enum() {
    let code = session_recovery_code();
    assert!(
        code.contains("super::liveness::") && code.contains("Verdict"),
        "session_recovery.rs must import Verdict from the liveness module"
    );
}

// ── Self-tests: each needle must have a real production occurrence ───────────

/// Every needle above must survive comment, literal, and `#[cfg(test)]`
/// stripping in the guarded file.
///
/// A presence assertion whose needle only ever appeared in prose is already
/// vacuous, and tightening the matcher would only convert a silent pass into a
/// silent failure. This names the needles so that regression is loud.
#[test]
fn every_guarded_needle_has_a_production_occurrence_today() {
    let code = session_recovery_code();
    let missing: Vec<&str> = [
        "classify_task_liveness",
        "Verdict::Live",
        "Verdict::Slow",
        "LivenessOutcome::KillNoop",
        "LivenessEvidenceSnapshot",
        "LivenessRepository",
        "ClassificationResult",
        "classify_session_exit_liveness",
        "super::liveness::",
    ]
    .into_iter()
    .filter(|needle| !code.contains(needle))
    .collect();
    assert!(
        missing.is_empty(),
        "these guards no longer have a production code occurrence to assert on, so they \
         are vacuous rather than green: {missing:?}"
    );
}

/// The pre-fix guard's exact failure mode, demonstrated against the real file.
///
/// Rename every non-comment occurrence of `classify_task_liveness` — the whole
/// classifier gate, calls and definition alike — and the old
/// `SESSION_RECOVERY_SRC.contains(…)` assertion still passes on the surviving
/// comments. The fixed predicate goes false. If this test ever stops holding
/// because the comments were deleted, the guard is no weaker; it is this
/// demonstration that has expired.
#[test]
fn the_old_whole_file_match_survived_deleting_every_real_call() {
    const NEEDLE: &str = "classify_task_liveness";
    let mutated = SESSION_RECOVERY_SRC
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                line.to_owned()
            } else {
                line.replace(NEEDLE, "decide_from_raw_signals")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        mutated.contains(NEEDLE),
        "the demonstration needs at least one comment still naming {NEEDLE}"
    );
    assert!(
        !production_mentions(&mutated, NEEDLE),
        "the guard must not accept a comment as evidence that {NEEDLE} is called"
    );
}

#[test]
fn a_needle_in_production_code_is_seen() {
    let source = "\
async fn recover(&self, task_id: &str) {
    let classification = self.classify_task_liveness(task_id).await;
}
";
    assert!(production_mentions(source, "classify_task_liveness"));
}

#[test]
fn a_needle_only_in_a_comment_or_a_log_message_does_not_satisfy_the_guard() {
    // These are the exact shapes present in `session_recovery.rs` today.
    let comment = "\
async fn recover(&self) {
    // classify_task_liveness already persisted the KillNoop evidence.
    self.reap().await;
}
";
    assert!(!production_mentions(comment, "classify_task_liveness"));

    let doc = "\
/// Runs [`super::liveness::classify`] and returns a `ClassificationResult`.
async fn recover(&self) {}
";
    assert!(!production_mentions(doc, "ClassificationResult"));

    let log = "\
async fn recover(&self) {
    tracing::warn!(\"classify_task_liveness: pool query failed\");
}
";
    assert!(!production_mentions(log, "classify_task_liveness"));
}

#[test]
fn a_trailing_comment_does_not_launder_the_call_in_front_of_it() {
    let source = "\
async fn recover(&self, task_id: &str) {
    self.classify_task_liveness(task_id).await; // consulted, not trusted
}
";
    assert!(production_mentions(source, "classify_task_liveness"));
}

#[test]
fn a_needle_only_inside_a_cfg_test_module_does_not_satisfy_the_guard() {
    let source = "\
async fn recover(&self) {
    self.reap().await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn classification() {
        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, Verdict::Live);
    }
}
";
    assert!(!production_mentions(source, "Verdict::Live"));
}

#[test]
fn production_code_after_a_cfg_test_module_still_counts() {
    // The `check-resize-reachability.sh` blind spot, in the presence direction:
    // a scan that stops at the first `#[cfg(test)]` marker reports the real call
    // below it as missing.
    let source = "\
#[cfg(test)]
mod fixtures {
    fn evidence() -> LivenessEvidence { LivenessEvidence::default() }
}

async fn recover(&self, task_id: &str) {
    let classification = self.classify_task_liveness(task_id).await;
}
";
    assert!(production_mentions(source, "classify_task_liveness"));
}

#[test]
fn a_cfg_test_field_does_not_swallow_the_production_call_after_it() {
    let source = "\
struct Recovery {
    #[cfg(test)]
    test_use_live_credential_resolution: bool,
    db: Db,
}

async fn recover(&self, task_id: &str) {
    let classification = self.classify_task_liveness(task_id).await;
}
";
    assert!(production_mentions(source, "classify_task_liveness"));
}
