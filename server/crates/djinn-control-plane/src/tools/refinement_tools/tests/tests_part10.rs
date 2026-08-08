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

async fn retry_fixture(
    scenario: djinn_db::test_support::TypedEvidenceRetryScenarioForTest,
    authority: djinn_db::test_support::TypedEvidenceRetryAuthorityForTest,
) -> (
    DjinnMcpServer,
    Database,
    djinn_core::models::Proposal,
    djinn_db::test_support::TypedEvidenceRetryFixtureForTest,
) {
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
    (server, db, proposal, fixture)
}

async fn retry_snapshot(
    db: &Database,
    proposal_id: &str,
    fixture: &djinn_db::test_support::TypedEvidenceRetryFixtureForTest,
) -> djinn_db::test_support::TypedEvidenceRetrySnapshotForTest {
    djinn_db::test_support::typed_evidence_retry_snapshot_for_test(
        db,
        proposal_id,
        &fixture.finding_id,
        &fixture.prior_spike_task_id,
    )
    .await
}

async fn retry_call(
    server: &DjinnMcpServer,
    caller_user_id: &str,
    fixture: &djinn_db::test_support::TypedEvidenceRetryFixtureForTest,
) -> serde_json::Value {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(caller_user_id.to_owned()), async {
            server
                .dispatch_tool(
                    "proposal_refinement_retry_evidence",
                    serde_json::json!({
                        "finding_id": fixture.finding_id,
                        "failed_transition_id": fixture.failed_transition_id,
                    }),
                )
                .await
                .unwrap()
        })
        .await
}

async fn retry_call_after_barrier(
    server: &DjinnMcpServer,
    caller_user_id: String,
    finding_id: String,
    failed_transition_id: String,
    barrier: Arc<tokio::sync::Barrier>,
) -> serde_json::Value {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(caller_user_id), async {
            barrier.wait().await;
            server
                .dispatch_tool(
                    "proposal_refinement_retry_evidence",
                    serde_json::json!({
                        "finding_id": finding_id,
                        "failed_transition_id": failed_transition_id,
                    }),
                )
                .await
                .unwrap()
        })
        .await
}

async fn disposition_fixture() -> (
    DjinnMcpServer,
    Database,
    djinn_core::models::Proposal,
    djinn_db::test_support::TypedEvidenceDispositionFixtureForTest,
) {
    let (server, db, proposal, user_id, judge_task_id) = setup_demand_test().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let project_id = repo.targets(&proposal.id).await.unwrap()[0]
        .project_id
        .clone();
    let fixture = djinn_db::test_support::materialize_typed_evidence_disposition_fixture_for_test(
        &db,
        &proposal.id,
        &project_id,
        &judge_task_id,
        &user_id,
    )
    .await;
    (server, db, proposal, fixture)
}

async fn disposition_snapshot(
    db: &Database,
    proposal_id: &str,
) -> djinn_db::test_support::TypedEvidenceDispositionSnapshotForTest {
    djinn_db::test_support::typed_evidence_disposition_snapshot_for_test(db, proposal_id).await
}

async fn disposition_call(
    server: &DjinnMcpServer,
    caller_user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(caller_user_id.to_owned()), async {
            server.dispatch_tool(tool, args).await.unwrap()
        })
        .await
}

// Fold through the public proposal handler; fixture SQL is not folding authority.
async fn disposition_folding_revision(
    server: &DjinnMcpServer,
    db: &Database,
    proposal: &djinn_core::models::Proposal,
    user_id: &str,
    authority_task_id: &str,
) -> i32 {
    djinn_db::test_support::set_proposal_author_for_test(db, &proposal.id, user_id).await;
    djinn_db::test_support::switch_to_advocate_authority_for_test(db, authority_task_id).await;
    let response = disposition_call(
        server,
        user_id,
        "proposal_update",
        serde_json::json!({
            "id": proposal.id,
            "body": format!("{}\n\nAdvocate committed evidence fold.", proposal.body),
        }),
    )
    .await;
    assert!(response["error"].is_null(), "fold rejected: {response}");
    let revision = ProposalRepository::new(db.clone(), EventBus::noop())
        .revisions(&proposal.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|revision| revision.event_kind == "spec_revision")
        .map(|revision| revision.seq)
        .max()
        .expect("public Advocate fold committed a revision");
    djinn_db::test_support::switch_to_judge_authority_for_test(db, authority_task_id).await;
    revision
}

fn disposition_args(
    tool: &str,
    fixture: &djinn_db::test_support::TypedEvidenceDispositionFixtureForTest,
    revision: i32,
) -> serde_json::Value {
    let mut args = serde_json::json!({
        "finding_id": fixture.finding_id,
        "folding_revision": revision,
        "rationale": "Terminal evidence decision is recorded with a rationale.",
    });
    if tool.ends_with("resolve_evidence") {
        args["validation_result_id"] = serde_json::json!(fixture.validation_result_id);
    } else {
        args["withdrawal_is_non_load_bearing"] = serde_json::json!(true);
    }
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_disposition_successes_return_machine_fields_and_clear_legacy() {
    for tool in [
        "proposal_refinement_resolve_evidence",
        "proposal_refinement_withdraw_evidence",
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        if tool.ends_with("withdraw_evidence") {
            djinn_db::test_support::seed_stale_typed_evidence_disposition_for_test(
                &db,
                &fixture.finding_id,
            )
            .await;
        }
        let revision = disposition_folding_revision(
            &server,
            &db,
            &proposal,
            &fixture.caller_user_id,
            &fixture.authority_task_id,
        )
        .await;
        let before = disposition_snapshot(&db, &proposal.id).await;
        let response = disposition_call(
            &server,
            &fixture.caller_user_id,
            tool,
            disposition_args(tool, &fixture, revision),
        )
        .await;
        let expected = if tool.ends_with("resolve_evidence") {
            "resolved"
        } else {
            "withdrawn"
        };
        assert_eq!(response["accepted"], true, "{response}");
        assert_eq!(response["finding_id"], fixture.finding_id);
        assert!(response["disposition_id"].as_str().is_some());
        assert_eq!(response["disposition"], expected);
        assert_eq!(response["lifecycle"], expected);
        assert_eq!(response["folding_revision"], revision);
        assert_eq!(
            response["validation_result_id"],
            if expected == "resolved" {
                serde_json::json!(fixture.validation_result_id)
            } else {
                serde_json::Value::Null
            }
        );
        assert_eq!(
            response["outcome"],
            if expected == "resolved" {
                "resolved"
            } else {
                "unresolved"
            }
        );
        assert!(response["error"].is_null() && response["conflict_code"].is_null());
        let after = disposition_snapshot(&db, &proposal.id).await;
        assert_eq!(after.dispositions.len(), before.dispositions.len() + 1);
        assert_eq!(after.transitions.len(), before.transitions.len() + 1);
        assert_eq!(after.legacy_link_and_claim, (None, None));
        assert_eq!(
            after.findings,
            vec![(fixture.finding_id.clone(), expected.into())]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_disposition_dispatch_rejections_and_decode_failures_are_exactly_pure() {
    for (tool, mutation, code) in [
        (
            "proposal_refinement_resolve_evidence",
            "uncommitted",
            "committed_folding_revision_required",
        ),
        (
            "proposal_refinement_resolve_evidence",
            "inapplicable",
            "validation_result_inapplicable",
        ),
        (
            "proposal_refinement_withdraw_evidence",
            "empty",
            "rationale_required",
        ),
        (
            "proposal_refinement_withdraw_evidence",
            "false",
            "non_load_bearing_assertion_required",
        ),
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        let revision = disposition_folding_revision(
            &server,
            &db,
            &proposal,
            &fixture.caller_user_id,
            &fixture.authority_task_id,
        )
        .await;
        let before = disposition_snapshot(&db, &proposal.id).await;
        let mut args = disposition_args(tool, &fixture, revision);
        match mutation {
            "uncommitted" => args["folding_revision"] = serde_json::json!(revision + 100),
            "inapplicable" => {
                args["validation_result_id"] = serde_json::json!(uuid::Uuid::now_v7().to_string())
            }
            "empty" => args["rationale"] = serde_json::json!(""),
            "false" => args["withdrawal_is_non_load_bearing"] = serde_json::json!(false),
            _ => unreachable!(),
        }
        let response = disposition_call(&server, &fixture.caller_user_id, tool, args).await;
        assert_eq!(response["accepted"], false, "{mutation}: {response}");
        assert_eq!(response["conflict_code"], code, "{mutation}: {response}");
        assert_eq!(
            disposition_snapshot(&db, &proposal.id).await,
            before,
            "{mutation} mutated state"
        );
    }
    for tool in [
        "proposal_refinement_resolve_evidence",
        "proposal_refinement_withdraw_evidence",
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        let before = disposition_snapshot(&db, &proposal.id).await;
        let result = server
            .dispatch_tool(tool, serde_json::json!({"finding_id": fixture.finding_id}))
            .await;
        assert!(result.is_err(), "required field omission must fail decode");
        assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_disposition_unauthorized_and_duplicate_terminal_calls_preserve_snapshot() {
    let (server, db, proposal, fixture) = disposition_fixture().await;
    let revision = disposition_folding_revision(
        &server,
        &db,
        &proposal,
        &fixture.caller_user_id,
        &fixture.authority_task_id,
    )
    .await;
    let before = disposition_snapshot(&db, &proposal.id).await;
    let denied = disposition_call(
        &server,
        &uuid::Uuid::now_v7().to_string(),
        "proposal_refinement_resolve_evidence",
        disposition_args("proposal_refinement_resolve_evidence", &fixture, revision),
    )
    .await;
    assert_eq!(denied["conflict_code"], "unauthorized");
    assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
    let accepted = disposition_call(
        &server,
        &fixture.caller_user_id,
        "proposal_refinement_resolve_evidence",
        disposition_args("proposal_refinement_resolve_evidence", &fixture, revision),
    )
    .await;
    assert_eq!(accepted["accepted"], true);
    let terminal = disposition_snapshot(&db, &proposal.id).await;
    let duplicate = disposition_call(
        &server,
        &fixture.caller_user_id,
        "proposal_refinement_resolve_evidence",
        disposition_args("proposal_refinement_resolve_evidence", &fixture, revision),
    )
    .await;
    assert_eq!(duplicate["accepted"], false);
    assert_eq!(duplicate["conflict_code"], "invalid_lifecycle");
    assert_eq!(disposition_snapshot(&db, &proposal.id).await, terminal);
}

fn assert_retry_allocation(
    before: &djinn_db::test_support::TypedEvidenceRetrySnapshotForTest,
    after: &djinn_db::test_support::TypedEvidenceRetrySnapshotForTest,
    response: &serde_json::Value,
    fixture: &djinn_db::test_support::TypedEvidenceRetryFixtureForTest,
    authority: &str,
) {
    assert_eq!(response["accepted"], true, "{authority}: {response}");
    assert_eq!(response["finding_id"], fixture.finding_id, "{authority}");
    assert_eq!(response["sequence"], 2, "{authority}");
    assert_eq!(response["lifecycle"], "demanded", "{authority}");
    assert_eq!(response["replayed"], false, "{authority}");
    assert_eq!(after.tasks.len(), before.tasks.len() + 1, "{authority}");
    assert_eq!(
        after.attempts.len(),
        before.attempts.len() + 1,
        "{authority}"
    );
    assert_eq!(
        after.transitions.len(),
        before.transitions.len() + 1,
        "{authority}"
    );
    assert_eq!(
        after.planned_checks.len(),
        before.planned_checks.len() + 1,
        "{authority}"
    );
    assert_eq!(
        after.retry_idempotency_rows.len(),
        before.retry_idempotency_rows.len() + 1,
        "{authority}"
    );
    assert_eq!(
        after.debate_rows, before.debate_rows,
        "retry preserves debate history"
    );
    assert_eq!(
        after.lifecycle_events, before.lifecycle_events,
        "retry preserves lifecycle history"
    );
    assert_eq!(
        after.prior_task_status, "closed",
        "retry must not reopen terminal spike"
    );
    let attempt_id = response["attempt_id"]
        .as_str()
        .expect("accepted retry attempt id");
    let retry_task_id = response["spike_task_id"]
        .as_str()
        .expect("accepted retry task id");
    assert_eq!(
        after.attempts.last(),
        Some(&(attempt_id.to_owned(), 2, retry_task_id.to_owned()))
    );
    let retry_task = after
        .tasks
        .iter()
        .find(|(id, _, _, _, _)| id == retry_task_id)
        .expect("persisted retry task");
    assert_eq!(retry_task.1, "open");
    assert_eq!(retry_task.2, "spike");
    assert_eq!(retry_task.3, "architect");
    assert_eq!(
        retry_task.4,
        serde_json::json!(["refinement-evidence", "read-only"])
    );
    assert!(
        after
            .routing
            .iter()
            .any(|(id, role)| id == retry_task_id && role == "architect")
    );
    assert!(after.labels.iter().any(|(id, labels)| id == retry_task_id
        && labels == &serde_json::json!(["refinement-evidence", "read-only"])));
    assert_eq!(
        after
            .planned_checks
            .last()
            .map(|(_, ordinal, check, method)| (*ordinal, check.as_str(), method.as_str())),
        before
            .planned_checks
            .last()
            .map(|(_, ordinal, check, method)| (*ordinal, check.as_str(), method.as_str())),
        "retry copies planned checks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_retry_judge_and_advocate_allocate_one_architect_spike() {
    for (authority, name) in [
        (
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            "judge",
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Advocate,
            "advocate",
        ),
    ] {
        let (server, db, proposal, fixture) = retry_fixture(
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
            authority,
        )
        .await;
        let before = retry_snapshot(&db, &proposal.id, &fixture).await;
        let response = retry_call(&server, &fixture.caller_user_id, &fixture).await;
        assert_retry_allocation(
            &before,
            &retry_snapshot(&db, &proposal.id, &fixture).await,
            &response,
            &fixture,
            name,
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_evidence_sequential_duplicate_replays_without_mutation() {
    let (server, db, proposal, fixture) = retry_fixture(
        djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
        djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
    )
    .await;
    let before = retry_snapshot(&db, &proposal.id, &fixture).await;
    let first = retry_call(&server, &fixture.caller_user_id, &fixture).await;
    let allocated = retry_snapshot(&db, &proposal.id, &fixture).await;
    assert_retry_allocation(&before, &allocated, &first, &fixture, "judge");
    let replay = retry_call(&server, &fixture.caller_user_id, &fixture).await;
    assert_eq!(replay["accepted"], true);
    assert_eq!(replay["replayed"], true);
    for field in [
        "finding_id",
        "attempt_id",
        "spike_task_id",
        "sequence",
        "lifecycle",
    ] {
        assert_eq!(replay[field], first[field], "duplicate {field}");
    }
    assert_eq!(
        retry_snapshot(&db, &proposal.id, &fixture).await,
        allocated,
        "replay preserves every retry snapshot dimension"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_evidence_concurrent_duplicate_creates_no_orphan_task() {
    let (server, db, proposal, fixture) = retry_fixture(
        djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
        djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
    )
    .await;
    let before = retry_snapshot(&db, &proposal.id, &fixture).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (first, second) = tokio::join!(
        retry_call_after_barrier(
            &server,
            fixture.caller_user_id.clone(),
            fixture.finding_id.clone(),
            fixture.failed_transition_id.clone(),
            barrier.clone()
        ),
        retry_call_after_barrier(
            &server,
            fixture.caller_user_id.clone(),
            fixture.finding_id.clone(),
            fixture.failed_transition_id.clone(),
            barrier
        ),
    );
    assert_eq!(first["accepted"], true, "{first}");
    assert_eq!(second["accepted"], true, "{second}");
    assert_ne!(
        first["replayed"], second["replayed"],
        "one caller allocates and one replays"
    );
    for field in [
        "finding_id",
        "attempt_id",
        "spike_task_id",
        "sequence",
        "lifecycle",
    ] {
        assert_eq!(first[field], second[field], "concurrent duplicate {field}");
    }
    let after = retry_snapshot(&db, &proposal.id, &fixture).await;
    let allocation = if first["replayed"] == false {
        &first
    } else {
        &second
    };
    assert_retry_allocation(&before, &after, allocation, &fixture, "concurrent judge");
    assert_eq!(
        after
            .tasks
            .iter()
            .filter(|(id, _, _, _, _)| id == allocation["spike_task_id"].as_str().unwrap())
            .count(),
        1,
        "concurrent delivery creates no orphan second task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_evidence_rejections_preserve_complete_snapshot() {
    let cases = [
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::StaleFailure,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            "retry_requires_latest_failed_transition",
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::NonFailed,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            "retry_requires_latest_failed_transition",
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::OccupiedSlot,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
            "active_evidence_conflict",
        ),
        (
            djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
            djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Unauthorized,
            "unauthorized",
        ),
    ];
    for (scenario, authority, code) in cases {
        let (server, db, proposal, fixture) = retry_fixture(scenario, authority).await;
        let before = retry_snapshot(&db, &proposal.id, &fixture).await;
        let response = retry_call(&server, &fixture.caller_user_id, &fixture).await;
        assert_eq!(
            response["accepted"], false,
            "{scenario:?}/{authority:?}: {response}"
        );
        assert_eq!(
            response["conflict_code"], code,
            "{scenario:?}/{authority:?}: {response}"
        );
        assert_eq!(
            retry_snapshot(&db, &proposal.id, &fixture).await,
            before,
            "rejection preserves complete snapshot"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_evidence_unauthorized_replay_never_caches_authority() {
    let (server, db, proposal, fixture) = retry_fixture(
        djinn_db::test_support::TypedEvidenceRetryScenarioForTest::Failed,
        djinn_db::test_support::TypedEvidenceRetryAuthorityForTest::Judge,
    )
    .await;
    let allocated = retry_call(&server, &fixture.caller_user_id, &fixture).await;
    let before_unauthorized_replay = retry_snapshot(&db, &proposal.id, &fixture).await;
    let unauthorized = retry_call(&server, &uuid::Uuid::now_v7().to_string(), &fixture).await;
    assert_eq!(allocated["accepted"], true);
    assert_eq!(unauthorized["accepted"], false);
    assert_eq!(unauthorized["conflict_code"], "unauthorized");
    assert_eq!(
        retry_snapshot(&db, &proposal.id, &fixture).await,
        before_unauthorized_replay,
        "idempotency must not cache authority"
    );
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

#[tokio::test]
async fn disposition_handlers_resolve_and_withdraw_with_machine_fields() {
    for (tool, disposition, lifecycle, mut args) in [
        (
            "proposal_refinement_resolve_evidence",
            "resolved",
            "resolved",
            serde_json::json!({"rationale":"validation folded"}),
        ),
        (
            "proposal_refinement_withdraw_evidence",
            "withdrawn",
            "withdrawn",
            serde_json::json!({"rationale":"no longer load bearing", "withdrawal_is_non_load_bearing":true}),
        ),
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        let before = disposition_snapshot(&db, &proposal.id).await;
        args["finding_id"] = serde_json::json!(fixture.finding_id);
        args["folding_revision"] = serde_json::json!(1);
        if tool.ends_with("resolve_evidence") {
            args["validation_result_id"] = serde_json::json!(fixture.validation_result_id);
        }
        let response = disposition_call(&server, &fixture.caller_user_id, tool, args).await;
        let after = disposition_snapshot(&db, &proposal.id).await;
        assert_eq!(response["accepted"], true, "{response}");
        assert_eq!(response["finding_id"], fixture.finding_id);
        assert!(response["disposition_id"].as_str().is_some());
        assert_eq!(response["disposition"], disposition);
        assert_eq!(response["lifecycle"], lifecycle);
        assert_eq!(response["folding_revision"], 1);
        assert_eq!(
            response["validation_result_id"],
            if disposition == "resolved" {
                serde_json::json!(fixture.validation_result_id)
            } else {
                serde_json::Value::Null
            }
        );
        assert_eq!(
            response["outcome"],
            if disposition == "resolved" {
                "resolved"
            } else {
                "unresolved"
            }
        );
        assert_eq!(response["error"], serde_json::Value::Null);
        assert_eq!(response["conflict_code"], serde_json::Value::Null);
        assert_eq!(after.dispositions.len(), before.dispositions.len() + 1);
        assert_eq!(after.transitions.len(), before.transitions.len() + 1);
        assert_eq!(after.legacy_link_and_claim, (None, None));
    }
}

#[tokio::test]
async fn disposition_rejections_and_decode_errors_preserve_complete_snapshot() {
    let cases = [
        (
            "proposal_refinement_resolve_evidence",
            serde_json::json!({"rationale":"x","folding_revision":1,"validation_result_id":"missing"}),
            "validation_result_inapplicable",
        ),
        (
            "proposal_refinement_resolve_evidence",
            serde_json::json!({"rationale":"x","folding_revision":99,"validation_result_id":"fixture"}),
            "committed_folding_revision_required",
        ),
        (
            "proposal_refinement_withdraw_evidence",
            serde_json::json!({"rationale":"","folding_revision":1,"withdrawal_is_non_load_bearing":true}),
            "rationale_required",
        ),
        (
            "proposal_refinement_withdraw_evidence",
            serde_json::json!({"rationale":"x","folding_revision":1,"withdrawal_is_non_load_bearing":false}),
            "non_load_bearing_assertion_required",
        ),
    ];
    for (tool, mut args, code) in cases {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        args["finding_id"] = serde_json::json!(fixture.finding_id);
        if args["validation_result_id"] == "fixture" {
            args["validation_result_id"] = serde_json::json!(fixture.validation_result_id);
        }
        let before = disposition_snapshot(&db, &proposal.id).await;
        let response = disposition_call(&server, &fixture.caller_user_id, tool, args).await;
        assert_eq!(response["accepted"], false, "{response}");
        assert_eq!(response["conflict_code"], code, "{response}");
        assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
    }
    for tool in [
        "proposal_refinement_resolve_evidence",
        "proposal_refinement_withdraw_evidence",
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        let before = disposition_snapshot(&db, &proposal.id).await;
        let result = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(fixture.caller_user_id), async {
                server
                    .dispatch_tool(tool, serde_json::json!({"finding_id":fixture.finding_id}))
                    .await
            })
            .await;
        assert!(result.is_err(), "missing required fields must fail decode");
        assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
    }
}

#[tokio::test]
async fn disposition_unauthorized_and_duplicate_calls_do_not_mutate() {
    for tool in [
        "proposal_refinement_resolve_evidence",
        "proposal_refinement_withdraw_evidence",
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        let args = if tool.ends_with("resolve_evidence") {
            serde_json::json!({"finding_id":fixture.finding_id,"validation_result_id":fixture.validation_result_id,"folding_revision":1,"rationale":"x"})
        } else {
            serde_json::json!({"finding_id":fixture.finding_id,"folding_revision":1,"rationale":"x","withdrawal_is_non_load_bearing":true})
        };
        let before = disposition_snapshot(&db, &proposal.id).await;
        let denied = disposition_call(
            &server,
            &uuid::Uuid::now_v7().to_string(),
            tool,
            args.clone(),
        )
        .await;
        assert_eq!(denied["conflict_code"], "unauthorized");
        assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
        let accepted = disposition_call(&server, &fixture.caller_user_id, tool, args.clone()).await;
        assert_eq!(accepted["accepted"], true);
        let terminal = disposition_snapshot(&db, &proposal.id).await;
        let duplicate = disposition_call(&server, &fixture.caller_user_id, tool, args).await;
        assert_eq!(duplicate["accepted"], false);
        assert_eq!(disposition_snapshot(&db, &proposal.id).await, terminal);
    }
}

#[tokio::test]
async fn typed_evidence_disposition_isolates_decode_roles_and_stale_lifecycle() {
    for (tool, missing) in [
        ("proposal_refinement_resolve_evidence", "folding_revision"),
        (
            "proposal_refinement_resolve_evidence",
            "validation_result_id",
        ),
        ("proposal_refinement_withdraw_evidence", "rationale"),
        (
            "proposal_refinement_withdraw_evidence",
            "withdrawal_is_non_load_bearing",
        ),
    ] {
        let (server, db, proposal, fixture) = disposition_fixture().await;
        let before = disposition_snapshot(&db, &proposal.id).await;
        let mut args = disposition_args(tool, &fixture, 1);
        args.as_object_mut().unwrap().remove(missing);
        let result = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(fixture.caller_user_id.clone()), async {
                server.dispatch_tool(tool, args).await
            })
            .await;
        assert!(result.is_err(), "missing {missing} must reject decode");
        assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
    }
    for role in ["advocate", "adversary"] {
        for tool in [
            "proposal_refinement_resolve_evidence",
            "proposal_refinement_withdraw_evidence",
        ] {
            let (server, db, proposal, fixture) = disposition_fixture().await;
            if role == "advocate" {
                djinn_db::test_support::switch_to_advocate_authority_for_test(
                    &db,
                    &fixture.authority_task_id,
                )
                .await;
            } else {
                djinn_db::test_support::switch_to_adversary_authority_for_test(
                    &db,
                    &fixture.authority_task_id,
                )
                .await;
            }
            let before = disposition_snapshot(&db, &proposal.id).await;
            let response = disposition_call(
                &server,
                &fixture.caller_user_id,
                tool,
                disposition_args(tool, &fixture, 1),
            )
            .await;
            assert_eq!(
                response["conflict_code"], "unauthorized",
                "{role}/{tool}: {response}"
            );
            assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
        }
    }
    let (server, db, proposal, fixture) = disposition_fixture().await;
    djinn_db::test_support::seed_stale_typed_evidence_disposition_for_test(
        &db,
        &fixture.finding_id,
    )
    .await;
    let before = disposition_snapshot(&db, &proposal.id).await;
    let stale = disposition_call(
        &server,
        &fixture.caller_user_id,
        "proposal_refinement_resolve_evidence",
        disposition_args("proposal_refinement_resolve_evidence", &fixture, 1),
    )
    .await;
    assert_eq!(stale["conflict_code"], "invalid_lifecycle");
    assert_eq!(disposition_snapshot(&db, &proposal.id).await, before);
}
