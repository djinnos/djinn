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
async fn withdraw_evidence_rejections_are_judge_only_and_do_not_clear_legacy_link() {
    let (server, db, proposal, judge_user_id, judge_task_id) = setup_demand_test().await;
    let demand = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(judge_user_id.clone()), async {
            server
                .dispatch_tool(
                    "proposal_refinement_demand_evidence",
                    valid_demand_params(&proposal.id),
                )
                .await
                .unwrap()
        })
        .await;
    let finding_id = demand["result"]["finding_id"].as_str().unwrap().to_owned();
    let transition_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM typed_evidence_transitions WHERE finding_id=$1")
            .bind(&finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let disposition_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM typed_evidence_dispositions WHERE finding_id=$1")
            .bind(&finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let legacy_link: Option<String> =
        sqlx::query_scalar("SELECT linked_spike_task_id FROM proposals WHERE id=$1")
            .bind(&proposal.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    for args in [
        serde_json::json!({"finding_id": finding_id, "folding_revision": 1, "rationale": "", "withdrawal_is_non_load_bearing": true}),
        serde_json::json!({"finding_id": finding_id, "folding_revision": 1, "rationale": "not load bearing", "withdrawal_is_non_load_bearing": false}),
        serde_json::json!({"finding_id": finding_id, "folding_revision": 999, "rationale": "not load bearing", "withdrawal_is_non_load_bearing": true}),
    ] {
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(judge_user_id.clone()), async {
                server
                    .dispatch_tool("proposal_refinement_withdraw_evidence", args)
                    .await
                    .unwrap()
            })
            .await;
        assert_eq!(response["accepted"], false, "{response}");
        let link: Option<String> =
            sqlx::query_scalar("SELECT linked_spike_task_id FROM proposals WHERE id=$1")
                .bind(&proposal.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM typed_evidence_transitions WHERE finding_id=$1"
            )
            .bind(&finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
            transition_count
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM typed_evidence_dispositions WHERE finding_id=$1"
            )
            .bind(&finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
            disposition_count
        );
        assert_eq!(link, legacy_link);
    }
    sqlx::query("UPDATE tasks SET agent_type='advocate', refinement_role='advocate', refinement_phase='advocate_revision' WHERE id=$1").bind(&judge_task_id).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE refinement_dispatch_intents SET role='advocate', phase='advocate_revision' WHERE task_id=$1").bind(&judge_task_id).execute(db.pool()).await.unwrap();
    let response = djinn_core::auth_context::SESSION_USER_ID.scope(Some(judge_user_id), async { server.dispatch_tool("proposal_refinement_withdraw_evidence", serde_json::json!({"finding_id": finding_id, "folding_revision": 1, "rationale": "not load bearing", "withdrawal_is_non_load_bearing": true})).await.unwrap() }).await;
    assert_eq!(response["conflict_code"], "unauthorized");
    let link: Option<String> =
        sqlx::query_scalar("SELECT linked_spike_task_id FROM proposals WHERE id=$1")
            .bind(&proposal.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(link, legacy_link);
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
async fn retry_evidence_judge_and_advocate_allocate_one_architect_spike() {
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
