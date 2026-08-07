//! Supervisor CI-adjudication tests (proposal `nafu`, wave 4).
//!
//! The acceptance matrix asks this crate for six things: a valid repair
//! reopen, a valid diagnostic reopen, invalid/timeout conversion **only while
//! current**, atomic suppression after a head/lane change or a newer
//! pass/merge, exactly one worker for a valid current reopen, and unchanged
//! passing/park/supersede behaviour.
//!
//! Two conventions make those assertions evidence rather than labels.
//!
//! 1. Effects are **counted**, never named. Every application assertion goes
//!    through [`CiBoardEffect::counts`], so a variant renamed or a plan that
//!    quietly dispatched two workers fails. `expect_noop` additionally
//!    asserts all six counters are zero, which is what "no reopen, no worker
//!    dispatch, no mutation of the current route" means operationally.
//! 2. Every plan is exercised against **both** guard outcomes. A test that
//!    only ever saw a passing guard would prove nothing about suppression,
//!    and a suppression test that never saw the plan succeed would not prove
//!    the plan was capable of doing anything in the first place.

use serde_json::json;

use super::*;

// ── Fixtures ───────────────────────────────────────────────────────────────

/// A run id and a check name from the bundle; a grounded directive must cite
/// one of them.
const RUN_ID: &str = "run-90210";
const CHECK: &str = "Server Test / test";
/// A command that genuinely exists in repository/task context.
const REPO_COMMAND: &str = "cargo test -p djinn-coordinator --lib ci_routing";

/// The guard keys a real route carries. Wave 5 made these mandatory: a block
/// that cannot name its row cannot be guarded, and an unguardable route is
/// never applied.
fn guard_keys() -> CiRouteGuardKeys {
    CiRouteGuardKeys {
        subject: CiRouteSubject::task("t123"),
        provider_action_key: "pak-abc".to_owned(),
        tier2_lease_id: "lease-abc".to_owned(),
        identity: CiEvidenceIdentity {
            lane: CiLane::PrHead,
            pr_number: 907,
            pr_head_sha: "abc1234".to_owned(),
            run_id: 90210,
            run_head_sha: "abc1234".to_owned(),
            dequeue_id: None,
        },
    }
}

fn context(tier2_reason: CiTier2Reason) -> CiAdjudicationContext {
    CiAdjudicationContext {
        lane: CiLane::PrHead,
        origin_state: CiOriginState::PrDraft,
        guard: guard_keys(),
        tier2_reason,
        repository_commands: vec![
            REPO_COMMAND.to_owned(),
            "cargo clippy --workspace".to_owned(),
        ],
        evidence_references: vec![RUN_ID.to_owned(), CHECK.to_owned(), "abc1234".to_owned()],
    }
}

/// The ordinary causal-failure route: the only one on which park is legal.
fn causal_context() -> CiAdjudicationContext {
    context(CiTier2Reason::CausalFailure)
}

fn repair_payload() -> serde_json::Value {
    json!({
        "task_id": "t123",
        "decision": "reopen",
        "directive": format!(
            "`{CHECK}` in run {RUN_ID} fails at server/crates/djinn-db/src/lib.rs:118 because \
             the re-export list omits CiOriginState; add it."
        ),
        "verification_command": REPO_COMMAND,
    })
}

fn diagnose_payload(reason: &str) -> serde_json::Value {
    json!({
        "task_id": "t123",
        "decision": "reopen",
        "directive": format!(
            "Run {RUN_ID} shows `{CHECK}` failing with no log section captured; what remains \
             unknown is which assertion failed."
        ),
        "diagnostic_reason": reason,
    })
}

fn park_payload() -> serde_json::Value {
    json!({
        "task_id": "t123",
        "decision": "park",
        "park_dossier": {
            "hold_description": "The self-hosted runner pool cannot pull the build image.",
            "failure_analysis": format!(
                "Run {RUN_ID} failed in `{CHECK}` before checkout with an image-pull error on \
                 every attempt; no rerun and no code change in this repository can clear it."
            ),
            "recommended_action": "Escalate to the platform owner of the runner pool.",
        },
    })
}

fn supersede_payload() -> serde_json::Value {
    json!({
        "task_id": "t123",
        "decision": "supersede",
        "created_tasks": ["a1b2", "c3d4"],
    })
}

/// Assert an effect does nothing whatsoever.
#[track_caller]
fn expect_noop(effect: &CiBoardEffect) {
    assert!(effect.is_noop(), "expected a no-op, got {effect:?}");
    assert_eq!(
        effect.counts(),
        CiEffectCounts::default(),
        "a suppressed result must perform no transition, no worker dispatch, no mutation of the \
         current route, no further Lead session, no dependency edge, and no provider action"
    );
}

/// Assert an effect is exactly one board transition and exactly one worker.
#[track_caller]
fn expect_single_worker_reopen(effect: &CiBoardEffect) {
    let counts = effect.counts();
    assert_eq!(counts.board_transitions, 1, "expected one transition");
    assert_eq!(
        counts.worker_dispatches, 1,
        "a valid current reopen dispatches EXACTLY one worker"
    );
    assert_eq!(counts.current_route_mutations, 0);
    assert_eq!(counts.lead_sessions, 0);
    assert_eq!(counts.dependency_edges, 0);
    assert_eq!(counts.provider_actions, 0);
}

// ── Valid repair reopen ────────────────────────────────────────────────────

/// A grounded directive plus a command copied from repository context is a
/// repair: durable mode `repair`, no diagnostic reason, one worker.
#[test]
fn valid_repair_reopen_dispatches_exactly_one_worker() {
    let ctx = causal_context();
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&repair_payload()));

    assert_eq!(
        adjudication.rejection, None,
        "a valid repair is not replaced"
    );
    let CiLeadPlan::RepairReopen {
        ref verification_command,
        ..
    } = adjudication.plan
    else {
        panic!("expected a repair plan, got {:?}", adjudication.plan);
    };
    assert_eq!(verification_command, REPO_COMMAND);

    let resolution = durable_resolution(&adjudication);
    assert_eq!(resolution.outcome, CiRouteOutcome::RepairReopened);
    assert_eq!(resolution.reopen_mode, Some(CiReopenMode::Repair));
    assert_eq!(
        resolution.diagnostic_reason, None,
        "a repair forbids a diagnostic reason"
    );

    let effect = board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current);
    expect_single_worker_reopen(&effect);
    let CiBoardEffect::Reopen {
        origin_state,
        mode,
        ref verification_command,
        ..
    } = effect
    else {
        panic!("expected a reopen effect, got {effect:?}");
    };
    assert_eq!(
        origin_state,
        CiOriginState::PrDraft,
        "reopen from the recorded origin"
    );
    assert_eq!(mode, CiReopenMode::Repair);
    assert_eq!(verification_command.as_deref(), Some(REPO_COMMAND));
}

/// A command Lead invented — here from the job name — is not repository-valid,
/// so the repair is invalid and becomes a diagnosis citing
/// `no_repository_command`. The grounded directive survives; only the invented
/// command is dropped.
#[test]
fn invented_verification_command_downgrades_repair_to_diagnosis() {
    let ctx = causal_context();
    let mut payload = repair_payload();
    payload["verification_command"] = json!("cargo test --workspace server-test");

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));

    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::VerificationCommandNotRepositoryValid)
    );
    let CiLeadPlan::DiagnosticReopen {
        reason,
        ref directive,
    } = adjudication.plan
    else {
        panic!("expected a diagnosis, got {:?}", adjudication.plan);
    };
    assert_eq!(reason, CiDiagnosticReason::NoRepositoryCommand);
    assert!(
        directive.contains("cargo test --workspace server-test"),
        "the rejected command must be named so the worker knows what not to trust"
    );

    let resolution = durable_resolution(&adjudication);
    assert_eq!(resolution.reopen_mode, Some(CiReopenMode::Diagnose));
    assert_eq!(
        resolution.diagnostic_reason,
        Some(CiDiagnosticReason::NoRepositoryCommand)
    );

    // A downgraded repair is still a reopen: one worker, not zero.
    expect_single_worker_reopen(&board_effect(
        &ctx,
        &adjudication.plan,
        CiGuardOutcome::Current,
    ));
}

/// A command that merely shares a prefix with a known one is a new command.
#[test]
fn prefix_of_a_known_command_is_not_repository_valid() {
    let ctx = causal_context();
    let mut payload = repair_payload();
    payload["verification_command"] = json!("cargo test -p djinn-coordinator");

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::VerificationCommandNotRepositoryValid),
        "a truncated command is not the command that was copied"
    );
}

/// Re-indenting a copied command must not invalidate it — the check is on the
/// command, not on its whitespace.
#[test]
fn whitespace_normalized_command_stays_repository_valid() {
    let ctx = causal_context();
    let mut payload = repair_payload();
    payload["verification_command"] =
        json!("cargo  test -p djinn-coordinator\n  --lib   ci_routing");

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(adjudication.rejection, None);
    assert!(matches!(adjudication.plan, CiLeadPlan::RepairReopen { .. }));
}

/// A directive that cites nothing from the evidence bundle is not grounded,
/// whatever it asserts about itself.
#[test]
fn ungrounded_directive_cannot_be_a_repair() {
    let ctx = causal_context();
    let mut payload = repair_payload();
    payload["directive"] = json!("Fix the flaky test. I verified this against the CI evidence.");

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::DirectiveNotGrounded)
    );
    assert!(matches!(
        adjudication.plan,
        CiLeadPlan::DiagnosticReopen { .. }
    ));
}

/// Grounding may come from the `evidence` citation rather than the directive
/// prose.
#[test]
fn evidence_citation_grounds_a_directive_that_does_not_repeat_the_handle() {
    let ctx = causal_context();
    let mut payload = repair_payload();
    payload["directive"] = json!("Add CiOriginState to the re-export list in djinn-db/src/lib.rs.");
    payload["evidence"] = json!({
        "source": "ci_run",
        "summary": format!("`{CHECK}` failed to compile in run {RUN_ID}"),
        "reference_id": RUN_ID,
    });

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(adjudication.rejection, None);
    assert!(matches!(adjudication.plan, CiLeadPlan::RepairReopen { .. }));
}

// ── Valid diagnostic reopen ────────────────────────────────────────────────

/// Every reason in the closed set is accepted, produces the durable diagnose
/// mode, and carries no verification command.
#[test]
fn valid_diagnostic_reopen_is_accepted_for_every_closed_reason() {
    let ctx = causal_context();
    for reason in [
        CiDiagnosticReason::EvidenceIncomplete,
        CiDiagnosticReason::ProviderActionFailed,
        CiDiagnosticReason::NoGroundedRemedy,
        CiDiagnosticReason::NoRepositoryCommand,
    ] {
        let payload = diagnose_payload(reason.as_str());
        let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));

        assert_eq!(
            adjudication.rejection,
            None,
            "`{}` is inside the closed set",
            reason.as_str()
        );
        assert_eq!(
            adjudication.plan,
            CiLeadPlan::DiagnosticReopen {
                directive: payload["directive"].as_str().unwrap().to_owned(),
                reason,
            }
        );

        let resolution = durable_resolution(&adjudication);
        assert_eq!(resolution.outcome, CiRouteOutcome::DiagnosticReopened);
        assert_eq!(resolution.reopen_mode, Some(CiReopenMode::Diagnose));
        assert_eq!(resolution.diagnostic_reason, Some(reason));

        let effect = board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current);
        expect_single_worker_reopen(&effect);
        let CiBoardEffect::Reopen {
            mode,
            ref verification_command,
            ..
        } = effect
        else {
            panic!("expected a reopen effect, got {effect:?}");
        };
        assert_eq!(mode, CiReopenMode::Diagnose);
        assert_eq!(
            verification_command.as_ref(),
            None,
            "a diagnostic reopen forbids a verification command"
        );
    }
}

/// The set is closed: a plausible-looking reason outside it is not accepted.
#[test]
fn diagnostic_reason_outside_the_closed_set_is_rejected() {
    let ctx = causal_context();
    for bogus in ["flaky_infrastructure", "transient", "unknown", "timeout"] {
        let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&diagnose_payload(bogus)));
        assert_eq!(
            adjudication.rejection,
            Some(CiResultRejection::DiagnosticReasonUnknown),
            "`{bogus}` must not be accepted as a diagnostic reason"
        );
    }
}

/// The closed set this crate validates is exactly the durable enum's. The
/// `djinn-roles` prompt test restates the same four names for the model, and
/// this exhaustive match is what makes a fifth durable variant break the
/// build rather than silently diverge from the prompt.
#[test]
fn diagnostic_reason_set_matches_the_durable_enum() {
    let all = [
        CiDiagnosticReason::EvidenceIncomplete,
        CiDiagnosticReason::ProviderActionFailed,
        CiDiagnosticReason::NoGroundedRemedy,
        CiDiagnosticReason::NoRepositoryCommand,
    ];
    for reason in all {
        // Exhaustive: adding a variant to the durable enum fails to compile
        // here until it is added to `all` and to `lead.md`.
        let spelling = match reason {
            CiDiagnosticReason::EvidenceIncomplete => "evidence_incomplete",
            CiDiagnosticReason::ProviderActionFailed => "provider_action_failed",
            CiDiagnosticReason::NoGroundedRemedy => "no_grounded_remedy",
            CiDiagnosticReason::NoRepositoryCommand => "no_repository_command",
        };
        assert_eq!(reason.as_str(), spelling);
        assert_eq!(CiDiagnosticReason::parse(spelling).unwrap(), reason);
    }
}

// ── Mutual exclusivity ─────────────────────────────────────────────────────

/// Both plans at once, or neither, states no plan. Neither half is believed.
#[test]
fn reopen_carrying_both_or_neither_plan_is_ambiguous() {
    let ctx = causal_context();

    let mut both = repair_payload();
    both["diagnostic_reason"] = json!("no_grounded_remedy");
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&both));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::ReopenPlanAmbiguous),
        "a repair carrying a diagnostic reason is not a valid repair"
    );

    let mut neither = repair_payload();
    neither
        .as_object_mut()
        .unwrap()
        .remove("verification_command");
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&neither));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::ReopenPlanAmbiguous),
        "a reopen with neither a command nor a reason states no plan"
    );
}

/// An empty-string command or reason is absence, not a plan signal.
#[test]
fn blank_plan_fields_do_not_signal_a_plan() {
    let ctx = causal_context();
    let mut payload = diagnose_payload("no_grounded_remedy");
    payload["verification_command"] = json!("   ");

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(
        adjudication.rejection, None,
        "a blank command must not make a valid diagnosis ambiguous"
    );
    assert!(matches!(
        adjudication.plan,
        CiLeadPlan::DiagnosticReopen {
            reason: CiDiagnosticReason::NoGroundedRemedy,
            ..
        }
    ));
}

// ── Passing-only approve ───────────────────────────────────────────────────

/// Non-passing CI can never be approved. Reaching the adjudicator at all
/// means the evidence was not passing.
#[test]
fn approve_on_a_ci_route_becomes_a_diagnosis() {
    let ctx = causal_context();
    for decision in ["approve", "approve_conflict"] {
        let payload = json!({
            "task_id": "t123",
            "decision": decision,
            "evidence": {"source": "ci_run", "summary": format!("run {RUN_ID} is only flaky")},
        });
        let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
        assert_eq!(
            adjudication.rejection,
            Some(CiResultRejection::ApprovedNonPassingCi),
            "`{decision}` must not apply to non-passing CI"
        );
        assert!(matches!(
            adjudication.plan,
            CiLeadPlan::DiagnosticReopen { .. }
        ));
    }
}

// ── Narrow, cited park ─────────────────────────────────────────────────────

/// A cited infrastructure dead-end on a causal route parks, and parks alone —
/// no worker.
#[test]
fn cited_infrastructure_dead_end_parks_without_a_worker() {
    let ctx = causal_context();
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&park_payload()));
    assert_eq!(adjudication.rejection, None);

    let resolution = durable_resolution(&adjudication);
    assert_eq!(resolution.outcome, CiRouteOutcome::Parked);
    assert!(
        resolution
            .park_justification
            .as_deref()
            .is_some_and(|j| j.contains(RUN_ID)),
        "the durable park justification must carry the citation"
    );

    let counts = board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current).counts();
    assert_eq!(counts.board_transitions, 1);
    assert_eq!(counts.worker_dispatches, 0, "a park dispatches no worker");
}

/// Uncertainty, an exhausted budget, a provider error, and an unknown outcome
/// reopen rather than park — and the fallback's reason names the actual
/// boundary rather than a generic one.
#[test]
fn the_four_reopen_not_park_routes_never_park() {
    for (reason, expected) in [
        (
            CiTier2Reason::EvidenceUnknown,
            CiDiagnosticReason::EvidenceIncomplete,
        ),
        (
            CiTier2Reason::ProviderActionFailed,
            CiDiagnosticReason::ProviderActionFailed,
        ),
        (
            CiTier2Reason::OutcomeUnknown,
            CiDiagnosticReason::ProviderActionFailed,
        ),
        (
            CiTier2Reason::RetryExhausted,
            CiDiagnosticReason::NoGroundedRemedy,
        ),
    ] {
        let ctx = context(reason);
        let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&park_payload()));
        assert_eq!(
            adjudication.rejection,
            Some(CiResultRejection::ParkUnavailableForRoute),
            "park must be unavailable for `{}`",
            reason.as_str()
        );
        assert_eq!(
            adjudication.plan,
            CiLeadPlan::DiagnosticReopen {
                directive: match &adjudication.plan {
                    CiLeadPlan::DiagnosticReopen { directive, .. } => directive.clone(),
                    other => panic!("expected a diagnosis, got {other:?}"),
                },
                reason: expected,
            },
            "the fallback must cite the route's actual unknown boundary"
        );
        // And it is a reopen, so it still costs exactly one worker.
        expect_single_worker_reopen(&board_effect(
            &ctx,
            &adjudication.plan,
            CiGuardOutcome::Current,
        ));
    }
}

/// A dossier that asserts a dead-end without naming this route's evidence is
/// not a citation.
#[test]
fn uncited_park_dossier_becomes_a_diagnosis() {
    let ctx = causal_context();
    let mut payload = park_payload();
    payload["park_dossier"]["failure_analysis"] =
        json!("The infrastructure is broken and nothing can be done about it.");
    payload["park_dossier"]["hold_description"] = json!("Blocked on infrastructure.");

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::ParkNotCited)
    );
    assert!(matches!(
        adjudication.plan,
        CiLeadPlan::DiagnosticReopen { .. }
    ));
}

/// A park with no dossier at all is not a park.
#[test]
fn park_without_a_dossier_becomes_a_diagnosis() {
    let ctx = causal_context();
    let adjudication = adjudicate(
        &ctx,
        LeadResponse::Submitted(&json!({"task_id": "t123", "decision": "park"})),
    );
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::ParkNotCited)
    );
}

// ── Unchanged supersede ────────────────────────────────────────────────────

/// Supersede keeps its existing contract, with no CI-specific widening: a
/// non-empty replacement list is required and nothing else changes.
#[test]
fn supersede_behaviour_is_unchanged_by_ci_routing() {
    let ctx = causal_context();

    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&supersede_payload()));
    assert_eq!(adjudication.rejection, None);
    assert_eq!(
        adjudication.plan,
        CiLeadPlan::Supersede {
            replacement_task_ids: vec!["a1b2".to_owned(), "c3d4".to_owned()],
        }
    );
    let counts = board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current).counts();
    assert_eq!(counts.board_transitions, 1);
    assert_eq!(counts.worker_dispatches, 0);

    let mut empty = supersede_payload();
    empty["created_tasks"] = json!([]);
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&empty));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::SupersedeWithoutReplacements)
    );
}

// ── Invalid / unsupported / timed-out conversion ───────────────────────────

/// Each of the four non-results becomes exactly one diagnostic reopen while
/// the route is current — never a second Lead session, never a silent drop.
#[test]
fn invalid_unsupported_and_timed_out_results_convert_to_one_diagnosis() {
    let ctx = causal_context();
    let unknown = json!({"task_id": "t123", "decision": "retrigger"});
    let cases: [(LeadResponse<'_>, CiResultRejection); 4] = [
        (
            LeadResponse::Submitted(&unknown),
            CiResultRejection::UnknownDecision,
        ),
        (
            LeadResponse::Unsupported("submit_work"),
            CiResultRejection::UnsupportedFinalizeTool,
        ),
        (LeadResponse::Missing, CiResultRejection::NoResult),
        (LeadResponse::TimedOut, CiResultRejection::TimedOut),
    ];

    for (response, expected) in cases {
        let adjudication = adjudicate(&ctx, response);
        assert_eq!(adjudication.rejection, Some(expected));
        assert!(
            matches!(adjudication.plan, CiLeadPlan::DiagnosticReopen { .. }),
            "{expected:?} must become a diagnostic reopen"
        );

        let effect = board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current);
        expect_single_worker_reopen(&effect);
        assert_eq!(
            effect.counts().lead_sessions,
            0,
            "a fallback must not request another Lead session"
        );
    }
}

/// `retrigger` and `requeue` have no representation: they are unknown
/// decisions, and they do not reach the provider.
#[test]
fn lead_cannot_request_a_provider_retry() {
    let ctx = causal_context();
    for retry in [
        "retrigger",
        "requeue",
        "rerun_failed_jobs",
        "enable_auto_merge",
    ] {
        let payload = json!({"task_id": "t123", "decision": retry});
        let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
        assert_eq!(
            adjudication.rejection,
            Some(CiResultRejection::UnknownDecision),
            "`{retry}` must not be a decision"
        );
        assert_eq!(
            board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current)
                .counts()
                .provider_actions,
            0,
            "no Lead result may produce a provider action"
        );
    }
}

/// No plan, valid or fallback, ever adds a dependency edge.
#[test]
fn no_plan_mutates_the_dependency_graph() {
    let ctx = causal_context();
    let mut with_blockers = repair_payload();
    with_blockers["blocked_by_add"] = json!(["e5f6"]);

    for plan in [
        adjudicate(&ctx, LeadResponse::Submitted(&with_blockers)).plan,
        adjudicate(
            &ctx,
            LeadResponse::Submitted(&diagnose_payload("no_grounded_remedy")),
        )
        .plan,
        adjudicate(&ctx, LeadResponse::Submitted(&park_payload())).plan,
        adjudicate(&ctx, LeadResponse::Submitted(&supersede_payload())).plan,
        adjudicate(&ctx, LeadResponse::TimedOut).plan,
    ] {
        assert_eq!(
            board_effect(&ctx, &plan, CiGuardOutcome::Current)
                .counts()
                .dependency_edges,
            0,
            "{plan:?} must add no dependency edge"
        );
    }
}

// -- Context plumbing -------------------------------------------------------

/// A complete route block, as the coordinator's `route_directive` writes it.
fn complete_route_block() -> serde_json::Value {
    json!({
        "ci_route": {
            "lane": "merge_group",
            "origin_state": "pr_review",
            "tier2_reason": "retry_exhausted",
            "subject_kind": "task",
            "subject_id": "t123",
            "provider_action_key": "pak-abc",
            "tier2_lease_id": "lease-abc",
            "pr_number": 907,
            "pr_head_sha": "abc1234",
            "run_id": 90210,
            "run_head_sha": "abc1234",
            "dequeue_id": "dq-1",
            "repository_commands": [REPO_COMMAND, "  ", ""],
            "evidence_references": [RUN_ID],
        }
    })
}

/// A block missing `field` -- every required key, one at a time.
fn block_without(field: &str) -> serde_json::Value {
    let mut block = complete_route_block();
    block["ci_route"]
        .as_object_mut()
        .expect("ci_route is an object")
        .remove(field)
        .unwrap_or_else(|| panic!("`{field}` must exist in the complete block to be removed"));
    block
}

/// Absence is the legacy path. That is the "feature disabled" row of the
/// mixed-version matrix and it must stay byte-for-byte the old behaviour.
#[test]
fn an_absent_route_block_is_the_legacy_path() {
    assert!(matches!(
        CiAdjudicationContext::read_arbiter_directive(None),
        CiDirectiveRead::NoRoute
    ));
    assert!(
        matches!(
            CiAdjudicationContext::read_arbiter_directive(Some(
                &json!({"directive": "do the thing"})
            )),
            CiDirectiveRead::NoRoute
        ),
        "an ordinary intervention directive carries no CI route"
    );
}

/// A complete block yields a context carrying **both** halves: the validation
/// inputs and the guard keys.
#[test]
fn a_complete_route_block_carries_the_guard_keys() {
    let CiDirectiveRead::Route(ctx) =
        CiAdjudicationContext::read_arbiter_directive(Some(&complete_route_block()))
    else {
        panic!("a complete block yields a context");
    };
    assert_eq!(ctx.lane, CiLane::MergeGroup);
    assert_eq!(ctx.origin_state, CiOriginState::PrReview);
    assert_eq!(ctx.tier2_reason, CiTier2Reason::RetryExhausted);
    assert_eq!(
        ctx.repository_commands,
        vec![REPO_COMMAND.to_owned()],
        "blank commands are not commands"
    );
    // Without these the guard cannot name the row it must resolve, which is
    // the whole reason wave 4 could not write the apply wiring.
    assert_eq!(ctx.guard.provider_action_key, "pak-abc");
    assert_eq!(ctx.guard.tier2_lease_id, "lease-abc");
    assert_eq!(ctx.guard.subject, CiRouteSubject::task("t123"));
    assert_eq!(ctx.guard.identity.pr_number, 907);
    assert_eq!(ctx.guard.identity.run_id, 90210);
    assert_eq!(ctx.guard.identity.dequeue_id.as_deref(), Some("dq-1"));
}

/// **Every** required key, removed one at a time, must yield `Malformed`.
///
/// Enumerated rather than sampled: a block that parses without
/// `tier2_lease_id` is a route whose Lead result would be applied against
/// whatever lease happens to be open, which is worse than not applying it.
#[test]
fn every_missing_required_key_is_malformed_not_legacy() {
    for field in [
        "lane",
        "origin_state",
        "tier2_reason",
        "subject_kind",
        "subject_id",
        "provider_action_key",
        "tier2_lease_id",
        "pr_number",
        "pr_head_sha",
        "run_id",
        "run_head_sha",
        "evidence_references",
    ] {
        let read = CiAdjudicationContext::read_arbiter_directive(Some(&block_without(field)));
        assert!(
            matches!(read, CiDirectiveRead::Malformed(missing) if missing == field),
            "a block without `{field}` must be Malformed, got {read:?}"
        );
    }
    // `dequeue_id` is the one optional key: the PR-head lane has none.
    assert!(matches!(
        CiAdjudicationContext::read_arbiter_directive(Some(&block_without("dequeue_id"))),
        CiDirectiveRead::Route(_)
    ));
    // An unparseable value is as unusable as an absent one.
    let mut bad_lane = complete_route_block();
    bad_lane["ci_route"]["lane"] = json!("not_a_lane");
    assert!(matches!(
        CiAdjudicationContext::read_arbiter_directive(Some(&bad_lane)),
        CiDirectiveRead::Malformed("lane")
    ));
}

/// An **empty** evidence bundle is refused rather than accepted-and-ignored.
///
/// This is the fail-closed half of the wave-5 grounding fix. Under the old
/// rule an empty list made `is_grounded` accept any non-blank text, so a
/// producer that forgot the references voided AC7's evidence requirement and
/// nothing failed.
#[test]
fn an_empty_evidence_bundle_is_malformed() {
    let mut block = complete_route_block();
    block["ci_route"]["evidence_references"] = json!([]);
    assert!(matches!(
        CiAdjudicationContext::read_arbiter_directive(Some(&block)),
        CiDirectiveRead::Malformed("evidence_references")
    ));
    // Blank strings are not references either.
    block["ci_route"]["evidence_references"] = json!(["", "   "]);
    assert!(matches!(
        CiAdjudicationContext::read_arbiter_directive(Some(&block)),
        CiDirectiveRead::Malformed("evidence_references")
    ));
}

/// A context with no evidence handles grounds **nothing**.
///
/// The mutation this kills: restore the old
/// `if self.evidence_references.is_empty() { return !text.trim().is_empty() }`
/// fallback and this test goes red while the rest of the module stays green --
/// which is exactly what happened before wave 5.
#[test]
fn an_empty_evidence_bundle_grounds_nothing() {
    let mut ctx = causal_context();
    ctx.evidence_references.clear();
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&repair_payload()));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::DirectiveNotGrounded),
        "with no handles to cite a directive cannot be grounded; it must not fall \
         back to `is not blank`"
    );
}

// ── The MCP surface ────────────────────────────────────────────────────────

/// The `submit_decision` schema must expose `diagnostic_reason` with exactly
/// the closed set, and must not have grown a retry rung.
#[test]
fn submit_decision_schema_carries_the_closed_diagnostic_reason_set() {
    let tool = djinn_mcp_extension::finalize_tools::tool_submit_decision();
    let schema = serde_json::to_value(&*tool.input_schema).expect("schema serializes");
    let properties = schema
        .get("properties")
        .expect("submit_decision has properties");

    let decisions: Vec<&str> = properties["decision"]["enum"]
        .as_array()
        .expect("decision is an enum")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(
        decisions,
        vec!["approve", "approve_conflict", "reopen", "park", "supersede"],
        "CI routing must not add a top-level Lead result"
    );

    let reasons: Vec<&str> = properties["diagnostic_reason"]["enum"]
        .as_array()
        .expect("diagnostic_reason must be a closed enum, not a free string")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(
        reasons,
        vec![
            CiDiagnosticReason::EvidenceIncomplete.as_str(),
            CiDiagnosticReason::ProviderActionFailed.as_str(),
            CiDiagnosticReason::NoGroundedRemedy.as_str(),
            CiDiagnosticReason::NoRepositoryCommand.as_str(),
        ],
        "the schema's reason set must be the durable enum's, in order"
    );
}

// ── The stage boundary ─────────────────────────────────────────────────────

/// The contract applies **only** when the coordinator dispatched Lead under a
/// CI route. Without one, every existing arbiter outcome is byte-for-byte
/// unchanged — that is the "new binary, feature disabled" row of the
/// mixed-version matrix.
#[test]
fn stage_routing_leaves_the_legacy_arbiter_path_untouched() {
    use crate::supervisor_impl::stage::lead_stage_outcome_routed;

    let approve = json!({
        "task_id": "t123",
        "decision": "approve",
        "evidence": {"source": "git diff + CI", "summary": "All 5 AC satisfied"},
    });
    assert!(
        matches!(
            lead_stage_outcome_routed("submit_decision", Some(&approve), false, None),
            StageOutcome::LeadApproved { .. }
        ),
        "a passing-CI approve with no route must still approve"
    );
    assert!(
        matches!(
            lead_stage_outcome_routed("submit_decision", Some(&park_payload()), false, None),
            StageOutcome::LeadParked { .. }
        ),
        "park with no route keeps its existing validation"
    );
    assert!(
        matches!(
            lead_stage_outcome_routed("submit_decision", Some(&supersede_payload()), false, None),
            StageOutcome::LeadSuperseded { .. }
        ),
        "supersede with no route keeps its existing validation"
    );
    // The legacy path still hard-fails a reopen with no verification command;
    // only a CI route converts it to a diagnosis.
    assert!(
        matches!(
            lead_stage_outcome_routed(
                "submit_decision",
                Some(&diagnose_payload("no_grounded_remedy")),
                false,
                None
            ),
            StageOutcome::Failed { .. }
        ),
        "outside a CI route, a reopen without a verification command is still rejected"
    );
}

/// With a route, the contract binds: approve becomes a diagnostic reopen, a
/// repair stays a reopen with its command, and a diagnosis reopens with none.
#[test]
fn stage_routing_applies_the_ci_contract_when_a_route_is_present() {
    use crate::supervisor_impl::stage::lead_stage_outcome_routed;

    let ctx = causal_context();
    let approve = json!({
        "task_id": "t123",
        "decision": "approve",
        "evidence": {"source": "ci_run", "summary": format!("run {RUN_ID} is only flaky")},
    });
    let outcome = lead_stage_outcome_routed("submit_decision", Some(&approve), false, Some(&ctx));
    let StageOutcome::LeadReopen {
        ref verification_command,
        ..
    } = outcome
    else {
        panic!("approving non-passing CI must reopen, got {outcome:?}");
    };
    assert!(verification_command.is_empty());

    let outcome = lead_stage_outcome_routed(
        "submit_decision",
        Some(&repair_payload()),
        false,
        Some(&ctx),
    );
    let StageOutcome::LeadReopen {
        ref verification_command,
        ..
    } = outcome
    else {
        panic!("a valid repair must reopen, got {outcome:?}");
    };
    assert_eq!(verification_command, REPO_COMMAND);

    // A session that never finalized is a diagnosis, not a `Failed` stage:
    // the proposal allows exactly one fallback and no second Lead session.
    assert!(
        matches!(
            lead_stage_outcome_routed("", None, false, Some(&ctx)),
            StageOutcome::LeadReopen { .. }
        ),
        "a missing result on a CI route becomes one diagnostic reopen"
    );
}

/// The cumulative arbitration budget outranks the CI contract. Wave 4 must
/// not become a way to buy another worker cycle after the ceiling is hit.
#[test]
fn exhausted_arbitration_budget_still_forces_a_terminal_disposition() {
    use crate::supervisor_impl::stage::lead_stage_outcome_routed;

    let ctx = causal_context();
    assert!(
        matches!(
            lead_stage_outcome_routed("submit_decision", Some(&repair_payload()), true, Some(&ctx)),
            StageOutcome::LeadParked { .. }
        ),
        "a CI repair cannot start another cycle once the arbitration budget is exhausted"
    );
}

/// The durable reopen mode is derived from the plan, never from the payload's
/// own claim about itself.
#[test]
fn reopen_mode_is_derived_from_the_validated_plan() {
    let ctx = causal_context();
    let repair = adjudicate(&ctx, LeadResponse::Submitted(&repair_payload())).plan;
    let diagnose = adjudicate(
        &ctx,
        LeadResponse::Submitted(&diagnose_payload("evidence_incomplete")),
    )
    .plan;

    assert_eq!(reopen_mode(&repair), Some(CiReopenMode::Repair));
    assert_eq!(reopen_mode(&diagnose), Some(CiReopenMode::Diagnose));
    assert_eq!(
        reopen_mode(&adjudicate(&ctx, LeadResponse::Submitted(&park_payload())).plan),
        None,
        "a park is not a reopen"
    );
    assert_eq!(
        reopen_mode(&adjudicate(&ctx, LeadResponse::Submitted(&supersede_payload())).plan),
        None,
        "a supersede is not a reopen"
    );
}

// ── The named acceptance fixture ───────────────────────────────────────────

/// `lead_ci_routing::stale_result_rejected_before_apply` — the fixture the
/// proposal names.
///
/// Every result Lead can produce is exercised against a **failed** atomic
/// guard: a valid repair, a valid diagnosis, a cited park, a supersede, an
/// invalid output, and a timeout. Each must be a complete no-op on the
/// current task and the current route.
///
/// The pairing is what makes it evidence: each case is first shown to *do*
/// something under a passing guard, so a plan that was inert for an unrelated
/// reason cannot pass this test by accident.
mod lead_ci_routing {
    use super::*;

    #[test]
    fn stale_result_rejected_before_apply() {
        let ctx = causal_context();
        let repair = repair_payload();
        let diagnose = diagnose_payload("evidence_incomplete");
        let park = park_payload();
        let supersede = supersede_payload();
        let unknown = json!({"task_id": "t123", "decision": "retrigger"});

        let cases: [(&str, LeadResponse<'_>); 6] = [
            ("repair", LeadResponse::Submitted(&repair)),
            ("diagnose", LeadResponse::Submitted(&diagnose)),
            ("park", LeadResponse::Submitted(&park)),
            ("supersede", LeadResponse::Submitted(&supersede)),
            ("invalid-output", LeadResponse::Submitted(&unknown)),
            ("timeout", LeadResponse::TimedOut),
        ];

        for (label, response) in cases {
            let adjudication = adjudicate(&ctx, response);

            // 1. The plan is capable of acting: under a passing guard it
            //    produces a real board effect.
            let applied = board_effect(&ctx, &adjudication.plan, CiGuardOutcome::Current);
            assert!(
                !applied.is_noop(),
                "{label}: the control arm must actually do something, or the stale arm proves \
                 nothing"
            );
            assert_eq!(
                applied.counts().board_transitions,
                1,
                "{label}: a current result is exactly one transition"
            );

            // 2. The same plan against a failed guard is a total no-op.
            let suppressed = board_effect(
                &ctx,
                &adjudication.plan,
                CiGuardOutcome::SupersededBeforeApply,
            );
            expect_noop(&suppressed);
        }
    }

    /// The guard's four defeat conditions — a head change, a lane or dequeue
    /// change, a newer passing observation, and a merged PR — all arrive here
    /// as the same single fact: `resolve_tier2_lease` returned `false`. This
    /// pins that mapping so a caller cannot invent a fifth interpretation.
    #[test]
    fn every_guard_defeat_reads_as_superseded_before_apply() {
        assert_eq!(
            CiGuardOutcome::from_resolve(false),
            CiGuardOutcome::SupersededBeforeApply,
            "a lost compare-and-set is a supersession, not a write failure to retry"
        );
        assert_eq!(CiGuardOutcome::from_resolve(true), CiGuardOutcome::Current);

        let ctx = causal_context();
        let repair = repair_payload();
        let plan = adjudicate(&ctx, LeadResponse::Submitted(&repair)).plan;
        expect_noop(&board_effect(
            &ctx,
            &plan,
            CiGuardOutcome::from_resolve(false),
        ));
    }
}
