use super::*;
// Human-readable admission rejections.
//
// `RefinementAdmissionError::AlreadyActive` used to be rendered as the literal
// Rust variant name `"AlreadyActive"`, which is what the user saw in an error
// toast after clicking "Send feedback for another round" on proposal y6q4.
// Every variant must now render a sentence that says what happened and what to
// do, with the machine-readable classification kept as a separate `code` field.

/// Assert on the actual strings: a message that is not a sentence, or that
/// leaks a Rust identifier, is the defect this test exists to catch.
fn assert_is_human_readable(rejection: &AdmissionRejection) {
    let message = &rejection.message;
    assert!(
        message.contains(' '),
        "{:?} is not a sentence: {message:?}",
        rejection.code
    );
    assert!(
        message.ends_with('.'),
        "{:?} message must be a complete sentence: {message:?}",
        rejection.code
    );
    for identifier in [
        "AlreadyActive",
        "AdmissionConflict",
        "GenerationConflict",
        "ProposalNotFound",
        "InvalidRequest",
        "RefinementAdmissionError",
    ] {
        assert!(
            !message.contains(identifier),
            "{:?} message leaks the Rust identifier {identifier}: {message:?}",
            rejection.code
        );
    }
    assert!(
        rejection
            .code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_'),
        "code must be a stable snake_case token, got {:?}",
        rejection.code
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_retry_fixture_covers_success_and_rejection_boundaries() {
    let scenarios = [
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            true,
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Advocate,
            true,
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::StaleFailure,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            false,
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::NonFailed,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            false,
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::OccupiedSlot,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            false,
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Unauthorized,
            false,
        ),
    ];
    for (scenario, authority, accepted) in scenarios {
        let (server, db, proposal, user_id, judge_task_id) = setup_demand_test().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let project_id = repo.targets(&proposal.id).await.unwrap()[0]
            .project_id
            .clone();
        let fixture = djinn_db::test_support::materialize_typed_evidence_retry_fixture_for_test(
            &db,
            &proposal.id,
            &project_id,
            &judge_task_id,
            &user_id,
            scenario,
            authority,
        )
        .await;
        let before = djinn_db::test_support::typed_evidence_retry_snapshot_for_test(
            &db,
            &proposal.id,
            &fixture.finding_id,
            &fixture.prior_spike_task_id,
        )
        .await;
        let response = djinn_core::auth_context::SESSION_USER_ID.scope(Some(fixture.caller_user_id.clone()), async {
            server.dispatch_tool("proposal_refinement_retry_evidence", serde_json::json!({"finding_id": fixture.finding_id.clone(), "failed_transition_id": fixture.failed_transition_id.clone()})).await.unwrap()
        }).await;
        assert_eq!(
            response["accepted"], accepted,
            "scenario {scenario:?}/{authority:?}: {response}"
        );
        let after = djinn_db::test_support::typed_evidence_retry_snapshot_for_test(
            &db,
            &proposal.id,
            &fixture.finding_id,
            &fixture.prior_spike_task_id,
        )
        .await;
        if accepted {
            assert_eq!(after.attempts.len(), before.attempts.len() + 1);
            assert_eq!(after.planned_checks.len(), before.planned_checks.len() + 1);
            assert_eq!(after.prior_task_status, "closed");
            assert_eq!(after.retry_idempotency_rows.len(), 1);
            assert!(after.routing.iter().any(|(_, role)| role == "architect"));
            assert!(
                after
                    .labels
                    .iter()
                    .any(|(_, labels)| labels.to_string().contains("read-only"))
            );
        } else {
            assert_eq!(before, after, "rejected retry must be immutable");
        }
    }
}

#[test]
fn every_admission_error_variant_renders_a_human_readable_message() {
    let variants = vec![
        RefinementAdmissionError::AlreadyActive {
            proposal_id: "p-1".into(),
            run_id: "r-1".into(),
        },
        RefinementAdmissionError::GenerationConflict {
            proposal_id: "p-1".into(),
            generation: 3,
        },
        RefinementAdmissionError::AdmissionConflict,
        RefinementAdmissionError::Database(djinn_db::Error::InvalidData("boom".into())),
        RefinementAdmissionError::ProposalNotFound {
            proposal_id: "p-1".into(),
        },
        RefinementAdmissionError::InvalidRequest("empty idempotency key".into()),
    ];
    let mut codes = Vec::new();
    for variant in &variants {
        let rejection = admission_rejection(variant);
        assert_is_human_readable(&rejection);
        codes.push(rejection.code);
    }
    codes.sort_unstable();
    let mut unique = codes.clone();
    unique.dedup();
    assert_eq!(codes, unique, "each variant needs its own machine code");
}

#[test]
fn already_active_tells_the_user_what_happened_and_what_to_do() {
    let rejection = admission_rejection(&RefinementAdmissionError::AlreadyActive {
        proposal_id: "019fa0bb-6174-7462-859c-9f0a5530e88c".into(),
        run_id: "run-1".into(),
    });
    assert_eq!(rejection.code, "already_active");
    assert_eq!(
        rejection.message,
        "A tribunal round is already running for this proposal. \
         Wait for it to finish (or stop it) before starting another."
    );
}

#[test]
fn proposal_not_found_names_the_proposal() {
    let rejection = admission_rejection(&RefinementAdmissionError::ProposalNotFound {
        proposal_id: "y6q4".into(),
    });
    assert_eq!(rejection.code, "proposal_not_found");
    assert!(rejection.message.contains("y6q4"), "{}", rejection.message);
}

#[test]
fn invalid_request_carries_the_underlying_detail() {
    let rejection = admission_rejection(&RefinementAdmissionError::InvalidRequest(
        "no targets".into(),
    ));
    assert_eq!(rejection.code, "invalid_request");
    assert!(
        rejection.message.contains("no targets"),
        "{}",
        rejection.message
    );
}
