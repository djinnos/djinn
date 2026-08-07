use super::*;

/// Helper to build a minimal "all-green" evidence packet.
fn live_evidence() -> LivenessEvidence {
    LivenessEvidence {
        pod_phase: Some(PodPhase::Running),
        activity: ActivitySignal::Active,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(Duration::from_secs(600)),
        extension_budget_exhausted: false,
        hard_runtime_deadline_exceeded: false,
        exit_code: None,

        handed_off_from_session_held_status: false,
        transient_provider_fault: false,
    }
}

// ── Precedence 1: terminal task state → noop ─────────────────────────

#[test]
fn terminal_task_closed_produces_kill_noop() {
    let mut ev = live_evidence();
    ev.db_task_status = Some(DbTaskStatus::Closed);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Live); // verdict moot
    assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
    assert_eq!(result.reason, Some(LivenessReason::None));
    assert!(!result.extension_eligible);
}

#[test]
fn terminal_task_wins_over_hard_runtime() {
    let mut ev = live_evidence();
    ev.db_task_status = Some(DbTaskStatus::Closed);
    ev.hard_runtime_deadline_exceeded = true;

    let result = classify(&ev);
    assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
    // Terminal task should NOT produce Timeout even though hard cap is set
    assert_ne!(result.outcome, Some(LivenessOutcome::Timeout));
}

#[test]
fn terminal_task_wins_over_dead_signals() {
    let mut ev = live_evidence();
    ev.db_task_status = Some(DbTaskStatus::Closed);
    ev.pod_phase = Some(PodPhase::Absent);
    ev.activity = ActivitySignal::NeverActive;

    let result = classify(&ev);
    assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
}

#[test]
fn terminal_task_wins_over_protocol_violation() {
    let mut ev = live_evidence();
    ev.db_task_status = Some(DbTaskStatus::Closed);
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.db_session_status = Some(DbSessionStatus::Running); // would be protocol violation if not terminal task
    ev.exit_code = Some(0);

    let result = classify(&ev);
    assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
}

// ── Precedence 2: hard runtime cap ───────────────────────────────────

#[test]
fn hard_runtime_cap_produces_dead_timeout() {
    let mut ev = live_evidence();
    ev.hard_runtime_deadline_exceeded = true;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Dead);
    assert_eq!(result.outcome, Some(LivenessOutcome::Timeout));
    assert_eq!(result.reason, Some(LivenessReason::HardRuntimeExceeded));
    assert!(!result.extension_eligible);
}

#[test]
fn hard_runtime_cap_forbids_extension_even_when_slow() {
    let mut ev = live_evidence();
    ev.hard_runtime_deadline_exceeded = true;
    ev.activity = ActivitySignal::Idle;
    ev.extension_budget_exhausted = false;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Dead);
    assert!(!result.extension_eligible);
}

// ── Precedence 3: protocol violation ─────────────────────────────────

#[test]
fn clean_exit_on_nonterminal_task_is_protocol_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    // Task not terminal, session still running
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.db_session_status = Some(DbSessionStatus::Running);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
    assert_eq!(result.outcome, Some(LivenessOutcome::Success));
    assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));
}

#[test]
fn nonzero_exit_on_nonterminal_task_is_protocol_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(137);
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.db_session_status = Some(DbSessionStatus::Running);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
    assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
    assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
}

/// Production shape of the 2026-07-29 `nr41` failure: refinement round 3's
/// Adversary died on `reply loop error: provider stream event failed:
/// display=server_is_overloaded ... provider internal error (status 500)`.
/// The session was recorded `failed`, which the exit path folds to
/// `(PodPhase::Failed, exit 1)` on an `in_progress` task — the exact input
/// that produced `verdict: "protocol_violation", outcome: Some(Crash),
/// reason: Some(NonzeroExitNonterminal)` and terminalized the live attempt.
///
/// With the provider signal preserved, the same input must classify as a
/// retryable environmental death instead.
#[test]
fn transient_provider_fault_is_not_a_protocol_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(1);
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.db_session_status = Some(DbSessionStatus::Running);
    ev.transient_provider_fault = true;

    let result = classify(&ev);
    assert_ne!(
        result.verdict,
        Verdict::ProtocolViolation,
        "an upstream 500 violates no protocol — the session did its part"
    );
    assert_ne!(
        result.outcome,
        Some(LivenessOutcome::Crash),
        "the provider crashed, not the run"
    );
    assert_ne!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
    assert_eq!(result.verdict, Verdict::Dead);
    assert_eq!(result.outcome, Some(LivenessOutcome::DeadReclaimed));
    assert_eq!(result.reason, Some(LivenessReason::TransientProviderFault));
    assert!(!result.extension_eligible);
}

/// The guard must not be weakened: without the provider signal, the very
/// same evidence is still convicted. If this ever passes for the wrong
/// reason — e.g. because the new rung swallowed the branch — the test above
/// would be meaningless.
#[test]
fn genuine_nonzero_crash_without_a_provider_fault_is_still_convicted() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(1);
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.db_session_status = Some(DbSessionStatus::Running);
    ev.transient_provider_fault = false;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
    assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
    assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
}

/// The new rung is scoped to an EXITED pod. A provider fault noted while the
/// pod is still running (a mid-session blip the worker recovered from) must
/// not force a `Dead` verdict on a live session.
#[test]
fn transient_provider_fault_does_not_kill_a_still_running_pod() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Running);
    ev.activity = ActivitySignal::Active;
    ev.transient_provider_fault = true;

    let result = classify(&ev);
    assert_eq!(
        result.verdict,
        Verdict::Live,
        "a running, active pod stays live regardless of a past provider blip"
    );
}

/// Terminal task state still outranks everything, including the new rung.
#[test]
fn terminal_task_wins_over_transient_provider_fault() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(1);
    ev.db_task_status = Some(DbTaskStatus::Closed);
    ev.transient_provider_fault = true;

    let result = classify(&ev);
    assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
}

/// The two exoneration terms are independent and must compose. #2748's
/// handoff evidence answers "was the task moved deliberately?"; this change's
/// provider evidence answers "did something external kill the run?". A round
/// that died on an upstream 500 leaves the task exactly where it was, so it
/// has NO handoff evidence — and must still be exonerated.
#[test]
fn transient_provider_fault_exonerates_without_any_handoff_evidence() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(1);
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.db_session_status = Some(DbSessionStatus::Running);
    ev.handed_off_from_session_held_status = false;
    ev.transient_provider_fault = true;

    let result = classify(&ev);
    assert_eq!(result.reason, Some(LivenessReason::TransientProviderFault));

    // …and the converse still holds: handoff evidence alone, with no provider
    // fault, keeps taking #2748's path rather than being relabelled.
    let mut handoff = live_evidence();
    handoff.pod_phase = Some(PodPhase::Succeeded);
    handoff.exit_code = Some(0);
    handoff.db_task_status = Some(DbTaskStatus::Open);
    handoff.db_session_status = Some(DbSessionStatus::Running);
    handoff.handed_off_from_session_held_status = true;
    handoff.transient_provider_fault = false;

    let result = classify(&handoff);
    assert_ne!(result.verdict, Verdict::ProtocolViolation);
    assert_ne!(result.reason, Some(LivenessReason::TransientProviderFault));
}

#[test]
fn succeeded_pod_unknown_exit_still_clean_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = None; // unknown
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.db_session_status = Some(DbSessionStatus::Running);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
    assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));
}

#[test]
fn terminal_session_with_absent_required_handoff_is_protocol_violation() {
    // Terminal session truth is not an exoneration: a completed exit whose
    // Required handoff is still absent is positive inconsistency.
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    ev.db_session_status = Some(DbSessionStatus::Completed);
    ev.activity = ActivitySignal::Active;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
}

/// Terminal session truth is evidence, not an exoneration. The durable
/// StageOutcome barrier has already made a Required handoff visible before this
/// terminal observation; an unsettled task is therefore one positive violation.
#[test]
fn terminal_session_status_matrix_classifies_truthfully() {
    for (session_status, pod_phase, exit_code, reason) in [
        (
            DbSessionStatus::Completed,
            PodPhase::Succeeded,
            0,
            LivenessReason::CleanExitNonterminal,
        ),
        (
            DbSessionStatus::Failed,
            PodPhase::Failed,
            1,
            LivenessReason::NonzeroExitNonterminal,
        ),
        (
            DbSessionStatus::Interrupted,
            PodPhase::Failed,
            1,
            LivenessReason::NonzeroExitNonterminal,
        ),
    ] {
        let mut ev = live_evidence();
        ev.pod_phase = Some(pod_phase);
        ev.exit_code = Some(exit_code);
        ev.db_session_status = Some(session_status);
        ev.db_task_status = Some(DbTaskStatus::InProgress);
        let result = classify(&ev);
        assert_eq!(
            result.verdict,
            Verdict::ProtocolViolation,
            "{session_status:?}"
        );
        assert_eq!(result.reason, Some(reason));
    }
}

#[test]
fn terminal_session_handoff_and_missing_evidence_fail_closed() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    ev.db_session_status = Some(DbSessionStatus::Completed);
    ev.db_task_status = Some(DbTaskStatus::Open);
    ev.handed_off_from_session_held_status = true;
    assert_ne!(classify(&ev).verdict, Verdict::ProtocolViolation);

    ev.handed_off_from_session_held_status = false;
    ev.db_session_status = None;
    assert_ne!(classify(&ev).verdict, Verdict::ProtocolViolation);
    ev.db_session_status = Some(DbSessionStatus::Completed);
    ev.db_task_status = None;
    assert_ne!(classify(&ev).verdict, Verdict::ProtocolViolation);
}

/// A required handoff failure does not bypass the higher-priority hard runtime
/// cap.
#[test]
fn hard_runtime_precedes_terminal_session_structural_exoneration() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    ev.db_session_status = Some(DbSessionStatus::Completed);
    ev.db_task_status = Some(DbTaskStatus::InProgress);
    ev.hard_runtime_deadline_exceeded = true;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Dead);
    assert_eq!(result.outcome, Some(LivenessOutcome::Timeout));
    assert_eq!(result.reason, Some(LivenessReason::HardRuntimeExceeded));
}

// ── Precedence 4: Dead ───────────────────────────────────────────────

#[test]
fn absent_pod_with_no_activity_is_dead() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Absent);
    ev.activity = ActivitySignal::Idle;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Dead);
    assert_eq!(result.outcome, Some(LivenessOutcome::DeadReclaimed));
}

#[test]
fn failed_pod_with_terminal_session_and_absent_handoff_is_protocol_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(1);
    ev.activity = ActivitySignal::Idle;
    // Persisted termination does not excuse an absent Required handoff.
    ev.db_session_status = Some(DbSessionStatus::Failed);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
    assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
    assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
}

#[test]
fn failed_pod_with_running_session_is_protocol_violation() {
    // A Failed pod while the DB session is still Running is structurally
    // inconsistent — the protocol-violation check fires before Dead.
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Failed);
    ev.exit_code = Some(137);
    ev.activity = ActivitySignal::Idle;
    ev.db_session_status = Some(DbSessionStatus::Running);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::ProtocolViolation);
    assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
    assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
}

#[test]
fn no_pod_phase_with_never_active_is_dead() {
    let mut ev = live_evidence();
    ev.pod_phase = None;
    ev.activity = ActivitySignal::NeverActive;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Dead);
}

#[test]
fn absent_pod_with_active_activity_is_not_dead() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Absent);
    ev.activity = ActivitySignal::Active;

    let result = classify(&ev);
    // Active activity overrides absent pod — session may be between pod
    // transitions. Classifier returns Live since no higher-precedence
    // condition matches.
    assert_ne!(result.verdict, Verdict::Dead);
}

// ── Precedence 5: Slow + extension eligibility ───────────────────────

#[test]
fn running_pod_with_idle_activity_is_slow() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Running);
    ev.activity = ActivitySignal::Idle;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Slow);
    assert!(result.extension_eligible);
    assert_eq!(result.outcome, None);
    assert_eq!(result.reason, None);
}

#[test]
fn slow_with_exhausted_budget_is_not_extension_eligible() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Running);
    ev.activity = ActivitySignal::Idle;
    ev.extension_budget_exhausted = true;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Slow);
    assert!(!result.extension_eligible);
    assert_eq!(result.outcome, Some(LivenessOutcome::SlowExtended));
    assert_eq!(
        result.reason,
        Some(LivenessReason::SlowExtensionBudgetExhausted)
    );
}

#[test]
fn running_pod_with_never_active_is_slow() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Running);
    ev.activity = ActivitySignal::NeverActive;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Slow);
    assert!(result.extension_eligible);
}

// ── Live default ─────────────────────────────────────────────────────

#[test]
fn all_green_is_live() {
    let ev = live_evidence();
    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Live);
    assert_eq!(result.outcome, None);
    assert_eq!(result.reason, None);
    assert!(!result.extension_eligible);
}

#[test]
fn pending_pod_with_active_signal_is_live() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Pending);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Live);
}

#[test]
fn unknown_pod_with_active_signal_is_live() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Unknown);

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Live);
}

// ── Edge cases ───────────────────────────────────────────────────────

#[test]
fn no_task_status_is_not_terminal() {
    let mut ev = live_evidence();
    ev.db_task_status = None;
    ev.pod_phase = Some(PodPhase::Running);
    ev.activity = ActivitySignal::Active;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Live);
    assert_ne!(result.outcome, Some(LivenessOutcome::KillNoop));
}

/// Absence is not guilt. A violation needs POSITIVE evidence on both axes,
/// so a missing session row can no longer convict an exited pod — the
/// asymmetry that made unknown evidence land in `ProtocolViolation`
/// deterministically.
#[test]
fn no_session_status_with_exited_pod_is_not_a_protocol_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    ev.db_session_status = None; // no session row

    let result = classify(&ev);
    assert_ne!(result.verdict, Verdict::ProtocolViolation);
    assert_ne!(result.outcome, Some(LivenessOutcome::ProtocolViolation));
}

/// The mirror image: a missing task row must not convict either. Before
/// this, `db_task_status = None` failed precedence 1's `is_some_and` guard
/// AND satisfied this branch, so it was a guaranteed violation.
#[test]
fn no_task_status_with_exited_pod_is_not_a_protocol_violation() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    ev.db_session_status = Some(DbSessionStatus::Running);
    ev.db_task_status = None; // no task row

    let result = classify(&ev);
    assert_ne!(result.verdict, Verdict::ProtocolViolation);
}

/// A session that exits with its task parked at a recorded handoff did its
/// job. Every `is_settled` status must be exonerated, on both a clean and a
/// crashing exit.
#[test]
fn exit_at_a_recorded_handoff_is_not_a_protocol_violation() {
    for status in [
        DbTaskStatus::NeedsTaskReview,
        DbTaskStatus::InTaskReview,
        DbTaskStatus::Approved,
        DbTaskStatus::PrDraft,
        DbTaskStatus::PrReview,
        DbTaskStatus::NeedsLeadIntervention,
        DbTaskStatus::InLeadIntervention,
    ] {
        for (phase, code) in [(PodPhase::Succeeded, 0), (PodPhase::Failed, 1)] {
            let mut ev = live_evidence();
            ev.pod_phase = Some(phase);
            ev.exit_code = Some(code);
            ev.db_session_status = Some(DbSessionStatus::Running);
            ev.db_task_status = Some(status);

            let result = classify(&ev);
            assert_ne!(
                result.verdict,
                Verdict::ProtocolViolation,
                "{status:?} is a recorded handoff, not a structural inconsistency"
            );
        }
    }
}

/// The detector must keep firing on the shape it exists for: a pod that
/// exited leaving its task still claimed or still queued, with a persisted
/// session status that has not settled.
#[test]
fn nonterminal_persisted_sessions_keep_unsettled_exit_controls() {
    // `Paused` remains nonterminal under the classifier's current semantics,
    // so it must retain the same structural checks as `Running`.
    for session_status in [DbSessionStatus::Running, DbSessionStatus::Paused] {
        for task_status in [DbTaskStatus::Open, DbTaskStatus::InProgress] {
            let mut clean = live_evidence();
            clean.pod_phase = Some(PodPhase::Succeeded);
            clean.exit_code = Some(0);
            clean.db_session_status = Some(session_status);
            clean.db_task_status = Some(task_status);
            let result = classify(&clean);
            assert_eq!(
                result.verdict,
                Verdict::ProtocolViolation,
                "clean exit: {session_status:?} session left {task_status:?} unsettled"
            );
            assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));

            let mut crashed = clean.clone();
            crashed.pod_phase = Some(PodPhase::Failed);
            crashed.exit_code = Some(1);
            let result = classify(&crashed);
            assert_eq!(
                result.verdict,
                Verdict::ProtocolViolation,
                "nonzero exit: {session_status:?} session left {task_status:?} unsettled"
            );
            assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
            assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
        }
    }
}

/// A non-settled status is ambiguous on its own. The task's own transition
/// record disambiguates: a last transition OUT of a session-held status
/// means a live session put the task there deliberately (the reviewer
/// rejection path, `in_task_review → open`), so the exiting session
/// completed its protocol and must NOT be convicted.
#[test]
fn handoff_off_a_session_held_status_exonerates_an_unsettled_task() {
    for status in [DbTaskStatus::Open, DbTaskStatus::InProgress] {
        let mut ev = live_evidence();
        ev.pod_phase = Some(PodPhase::Succeeded);
        ev.exit_code = Some(0);
        ev.db_session_status = Some(DbSessionStatus::Running);
        ev.db_task_status = Some(status);
        ev.handed_off_from_session_held_status = true;

        let result = classify(&ev);
        assert_ne!(
            result.verdict,
            Verdict::ProtocolViolation,
            "{status:?} reached by a deliberate handoff is not a violation"
        );
        assert_ne!(result.reason, Some(LivenessReason::CleanExitNonterminal));
    }
}

/// The statuses that only a live session holds. A transition OUT of one of
/// these is a handoff; a transition INTO one is just a claim.
#[test]
fn session_held_statuses_are_the_active_ones() {
    for held in ["in_progress", "in_task_review", "in_lead_intervention"] {
        assert!(is_session_held_status(held), "{held} is session-held");
    }
    for not_held in [
        "open",
        "needs_task_review",
        "approved",
        "pr_draft",
        "pr_review",
        "needs_lead_intervention",
        "closed",
    ] {
        assert!(
            !is_session_held_status(not_held),
            "{not_held} is not session-held"
        );
    }
}

/// Wire string for a [`DbTaskStatus`], via its own serde rename — so this
/// stays honest if a variant is renamed.
fn wire(status: DbTaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// TRIPWIRE. `is_settled` claims to list recorded handoffs, but two of its
/// entries are CLAIM statuses that only a live session holds — the exact
/// set [`is_session_held_status`] names. That overlap is deliberate and
/// load-bearing: see the `is_settled` doc for why narrowing it is unsafe
/// while the supervisor settles the session row BEFORE issuing the
/// reviewer's transition.
///
/// If this fails, someone changed one of the two lists. Read that doc
/// before "fixing" the test — the two helpers are one axis, and moving
/// either one moves the false-positive rate of the violation branch.
#[test]
fn is_settled_overlaps_session_held_on_exactly_the_two_claim_statuses() {
    let all = [
        DbTaskStatus::Open,
        DbTaskStatus::InProgress,
        DbTaskStatus::NeedsTaskReview,
        DbTaskStatus::InTaskReview,
        DbTaskStatus::Approved,
        DbTaskStatus::PrDraft,
        DbTaskStatus::PrReview,
        DbTaskStatus::NeedsLeadIntervention,
        DbTaskStatus::InLeadIntervention,
        DbTaskStatus::Closed,
    ];

    let overlap: Vec<String> = all
        .into_iter()
        .filter(|s| s.is_settled() && is_session_held_status(&wire(*s)))
        .map(wire)
        .collect();

    assert_eq!(
        overlap,
        vec!["in_task_review", "in_lead_intervention"],
        "a session-held status is a CLAIM, not a handoff destination; these \
         two are excused anyway only because the exit classifier races the \
         supervisor's transition RPC (see DbTaskStatus::is_settled)"
    );

    // The third session-held status is NOT excused — that asymmetry is the
    // whole reason the overlap above is suspicious rather than principled.
    assert!(
        !DbTaskStatus::InProgress.is_settled(),
        "in_progress is session-held AND unsettled; in_task_review is \
         session-held AND settled — the two lists disagree"
    );
}

/// CHARACTERIZATION of the hole, built from the real traced path rather
/// than from guesswork.
///
/// A reviewer that ends without calling `submit_review` returns a
/// non-terminal `StageOutcome::Failed` that performs NO task transition,
/// yet settles its session `completed` (the settlement keys on the reply
/// loop's `Ok(())`, not on the outcome), so the pod exits 0. The exit
/// classifier consequently sees precisely this packet — and returns
/// `Live` with no outcome at all. The abandonment is invisible in the
/// liveness ledger.
///
/// This is asserted, not endorsed. It is pinned so that any future change
/// to `is_settled` or to the precedence order surfaces here with the
/// reasoning attached, instead of silently flipping a production
/// false-positive rate nobody measured.
#[test]
fn no_verdict_reviewer_exit_is_invisible_to_the_exit_classifier() {
    // Exactly what classify_session_exit_liveness builds for a "completed"
    // session whose task never left in_task_review.
    let ev = LivenessEvidence {
        pod_phase: Some(PodPhase::Succeeded),
        activity: ActivitySignal::Idle,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InTaskReview),
        claim_ttl_remaining: None,
        extension_budget_exhausted: false,
        hard_runtime_deadline_exceeded: false,
        exit_code: Some(0),
        // Last transition was needs_task_review → in_task_review, i.e. INTO
        // a session-held status: a claim, which proves nothing.
        handed_off_from_session_held_status: false,
        // The reviewer exited 0 with no provider error at all — this scenario
        // is about an abandoned post, not an upstream fault.
        transient_provider_fault: false,
    };

    let result = classify(&ev);
    assert_eq!(
        result.verdict,
        Verdict::Live,
        "a reviewer that abandoned its post is currently recorded as live"
    );
    assert_eq!(result.outcome, None, "and carries no outcome at all");
    assert_eq!(result.reason, None);

    // The single term keeping it out of the violation branch.
    assert!(
        DbTaskStatus::InTaskReview.is_settled(),
        "is_settled is the only guard standing between this packet and \
         ProtocolViolation/clean_exit_nonterminal"
    );

    // And every later branch is structurally unreachable for a clean exit:
    // Succeeded is neither absent-or-failed (precedence 4) nor running
    // (precedence 5), so `Live` is the fall-through, not a judgement.
    let mut crashed = ev.clone();
    crashed.pod_phase = Some(PodPhase::Failed);
    crashed.exit_code = Some(1);
    assert_eq!(
        classify(&crashed).verdict,
        Verdict::Dead,
        "the same packet with a Failed pod DOES reach precedence 4 — only \
         the clean-exit shape falls all the way through"
    );
}

/// Explicit regression for #2748: a reviewer that rejected work back to the
/// rework queue did exactly what the protocol asks and must stay
/// exonerated. The task rests at `open` (unsettled), so the ONLY thing
/// separating it from a stranded worker is the recorded handoff off a
/// session-held status.
#[test]
fn reviewer_reject_handoff_to_open_stays_exonerated() {
    // in_task_review → open, driven by TransitionAction::TaskReviewReject.
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Succeeded);
    ev.exit_code = Some(0);
    ev.activity = ActivitySignal::Idle;
    ev.db_session_status = Some(DbSessionStatus::Running);
    ev.db_task_status = Some(DbTaskStatus::Open);
    ev.handed_off_from_session_held_status = true;

    let result = classify(&ev);
    assert_ne!(
        result.verdict,
        Verdict::ProtocolViolation,
        "a rejecting reviewer parks the task at `open` on purpose"
    );
    assert_ne!(result.reason, Some(LivenessReason::CleanExitNonterminal));

    // The discriminator must be the handoff evidence, not the status: drop
    // it and the very same packet convicts.
    let mut stranded = ev.clone();
    stranded.handed_off_from_session_held_status = false;
    assert_eq!(
        classify(&stranded).verdict,
        Verdict::ProtocolViolation,
        "without the recorded handoff the same resting status IS the \
         stranded-worker shape the detector exists for"
    );
}

#[test]
fn no_session_status_with_running_pod_is_live() {
    let mut ev = live_evidence();
    ev.pod_phase = Some(PodPhase::Running);
    ev.db_session_status = None;
    ev.activity = ActivitySignal::Active;

    let result = classify(&ev);
    assert_eq!(result.verdict, Verdict::Live);
}

#[test]
fn evidence_is_echoed_in_result() {
    let ev = live_evidence();
    let result = classify(&ev);
    // The evidence should be cloned into the result for audit
    assert_eq!(result.evidence.pod_phase, ev.pod_phase);
    assert_eq!(result.evidence.activity, ev.activity);
}

// ── Serialization round-trips ────────────────────────────────────────

#[test]
fn verdict_serde_round_trip() {
    for v in [
        Verdict::Live,
        Verdict::Slow,
        Verdict::Dead,
        Verdict::ProtocolViolation,
    ] {
        let json = serde_json::to_string(&v).unwrap();
        let back: Verdict = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}

#[test]
fn outcome_serde_round_trip() {
    let outcomes = [
        LivenessOutcome::Success,
        LivenessOutcome::Crash,
        LivenessOutcome::Timeout,
        LivenessOutcome::DeadReclaimed,
        LivenessOutcome::ProtocolViolation,
        LivenessOutcome::KillNoop,
        LivenessOutcome::SlowExtended,
    ];
    for o in outcomes {
        let json = serde_json::to_string(&o).unwrap();
        let back: LivenessOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back);
    }
}

#[test]
fn reason_as_str_matches_expected() {
    assert_eq!(
        LivenessReason::CleanExitNonterminal.as_str(),
        "clean_exit_nonterminal"
    );
    assert_eq!(
        LivenessReason::NonzeroExitNonterminal.as_str(),
        "nonzero_exit_nonterminal"
    );
    assert_eq!(
        LivenessReason::HardRuntimeExceeded.as_str(),
        "hard_runtime_exceeded"
    );
    assert_eq!(
        LivenessReason::SlowExtensionBudgetExhausted.as_str(),
        "slow_extension_budget_exhausted"
    );
    assert_eq!(LivenessReason::None.as_str(), "none");
}

#[test]
fn classification_result_serde_round_trip() {
    let ev = live_evidence();
    let result = classify(&ev);
    let json = serde_json::to_string(&result).unwrap();
    let back: ClassificationResult = serde_json::from_str(&json).unwrap();
    assert_eq!(result.verdict, back.verdict);
    assert_eq!(result.outcome, back.outcome);
    assert_eq!(result.reason, back.reason);
    assert_eq!(result.extension_eligible, back.extension_eligible);
}

// ── Display impls ────────────────────────────────────────────────────

#[test]
fn verdict_display() {
    assert_eq!(Verdict::Live.to_string(), "live");
    assert_eq!(Verdict::Slow.to_string(), "slow");
    assert_eq!(Verdict::Dead.to_string(), "dead");
    assert_eq!(Verdict::ProtocolViolation.to_string(), "protocol_violation");
}

#[test]
fn outcome_display() {
    assert_eq!(LivenessOutcome::Success.to_string(), "success");
    assert_eq!(LivenessOutcome::Crash.to_string(), "crash");
    assert_eq!(LivenessOutcome::Timeout.to_string(), "timeout");
    assert_eq!(LivenessOutcome::DeadReclaimed.to_string(), "dead_reclaimed");
    assert_eq!(
        LivenessOutcome::ProtocolViolation.to_string(),
        "protocol_violation"
    );
    assert_eq!(LivenessOutcome::KillNoop.to_string(), "kill_noop");
    assert_eq!(LivenessOutcome::SlowExtended.to_string(), "slow_extended");
}

// ── DbStatus helpers ─────────────────────────────────────────────────

#[test]
fn db_session_status_terminal() {
    assert!(DbSessionStatus::Completed.is_terminal());
    assert!(DbSessionStatus::Interrupted.is_terminal());
    assert!(DbSessionStatus::Failed.is_terminal());
    assert!(!DbSessionStatus::Running.is_terminal());
    assert!(!DbSessionStatus::Paused.is_terminal());
}

#[test]
fn db_task_status_terminal() {
    assert!(DbTaskStatus::Closed.is_terminal());
    assert!(!DbTaskStatus::Open.is_terminal());
    assert!(!DbTaskStatus::InProgress.is_terminal());
    assert!(!DbTaskStatus::Approved.is_terminal());
}
