// djinn:allow-oversize
//! Typed-evidence ingress coverage deliberately crosses the production Slot
//! activity boundary before exercising the coordinator's live and cold paths.

use crate::refinement::RefinementPhase;
use crate::refinement_dispatch::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use djinn_core::models::{DispatchPause, NeedsEvidenceClaim};
use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    refinement_liveness::RefinementParkKind,
};
use djinn_db::test_support::{
    CanonicalTypedEvidenceReturnOutcomeForTest, TypedEvidenceFindingSnapshotForTest,
    TypedEvidenceIngressFixtureForTest, TypedEvidenceValidationSnapshotForTest,
};
use djinn_db::{
    AdmitRefinementRunRequest, AtomicEvidenceRetryInput, DispatchPauseRepository,
    DispatchPauseTarget, EffectiveCreatorProvenance, ParkRefinementRunRequest, ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource, TaskRepository, TypedEvidenceRepository,
};
use djinn_slot::finalize_handlers::handle_submit_work;
use tokio_util::sync::CancellationToken;

struct Fixture {
    db: djinn_db::Database,
    project_id: String,
    user_id: String,
    proposal_id: String,
    spike_task_id: String,
    finding_id: String,
    delivery: TypedEvidenceIngressFixtureForTest,
}

/// Establish the durable Judge tuple required by the production atomic retry
/// API. Authorization itself remains inside the repository mutation.
async fn retry_authority(f: &Fixture, idempotency_key: &str) -> String {
    let proposals = ProposalRepository::new(f.db.clone(), EventBus::noop());
    let (run_id, generation) = match proposals
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: f.proposal_id.clone(),
            idempotency_key: idempotency_key.to_owned(),
            source: RefinementAdmissionSource::ExplicitStart {
                actor: "typed-evidence-failed-retry-conformance".to_owned(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit retry authority run")
    {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    };
    let task = TaskRepository::new(f.db.clone(), EventBus::noop())
        .create_in_project_with_provenance(
            &f.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&f.user_id),
                source_task_id: None,
                proposal_id: Some(&f.proposal_id),
            },
            "Retry authority",
            "Authorize the failed typed-evidence retry",
            "",
            "task",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create retry authority task");
    djinn_db::test_support::materialize_judge_authority_for_test(
        &f.db,
        &task.id,
        &run_id,
        generation.into(),
    )
    .await;
    task.id
}

/// Persist the park that a cold coordinator must rehydrate before its typed
/// evidence replay can advance the exact awaiting run into Advocate folding.
async fn park_awaiting_evidence(f: &Fixture, idempotency_key: &str) -> String {
    let proposals = ProposalRepository::new(f.db.clone(), EventBus::noop());
    let (run_id, generation) = match proposals
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: f.proposal_id.clone(),
            idempotency_key: idempotency_key.to_owned(),
            source: RefinementAdmissionSource::ExplicitStart {
                actor: "commit-before-resume-test".to_owned(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit durable awaiting-evidence run")
    {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    };
    assert!(
        proposals
            .park_refinement_run(ParkRefinementRunRequest {
                run_id: run_id.clone(),
                generation,
                kind: RefinementParkKind::AwaitingEvidence,
            })
            .await
            .expect("park exact durable run"),
        "fixture must durably await evidence"
    );
    run_id
}

async fn fixture(outcome: CanonicalTypedEvidenceReturnOutcomeForTest) -> Fixture {
    let db = crate::test_helpers::create_test_db();
    let refinement = seed_refinement_fixture(&db).await;
    let tasks = TaskRepository::new(db.clone(), EventBus::noop());
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    let spike_task_id = tasks
        .create_in_project_with_provenance(
            &refinement.project_id,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&refinement.user_id),
                source_task_id: None,
                proposal_id: None,
            },
            "Evidence spike",
            "Investigate typed evidence",
            "",
            "spike",
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create spike")
        .id;
    let claim = NeedsEvidenceClaim {
        question: "Is this claim supported?".into(),
        target_subsystem: "refinement".into(),
        spec_unknown_anchor: "typed evidence".into(),
        insufficient_in_session_research: "spike required".into(),
        expected_findings: "canonical return".into(),
        round: 1,
        against_revision_seq: 1,
        created_by_task_id: spike_task_id.clone(),
    };
    proposals
        .set_structured_needs_evidence_spike(&refinement.proposal_id, &spike_task_id, &claim)
        .await
        .expect("establish typed authority");
    let delivery = djinn_db::test_support::seed_canonical_typed_evidence_ingress_fixture_for_test(
        &db,
        &refinement.proposal_id,
        &spike_task_id,
        "parity-check",
        outcome,
    )
    .await;
    tasks
        .set_status_with_reason(&spike_task_id, "closed", Some("completed"))
        .await
        .expect("terminal producer task");
    Fixture {
        db,
        project_id: refinement.project_id,
        user_id: refinement.user_id,
        proposal_id: refinement.proposal_id,
        spike_task_id,
        finding_id: delivery.finding_id.clone(),
        delivery,
    }
}

/// Real Slot submission followed by capture of its committed durable payload.
async fn submitted_envelope(f: &Fixture, payload: serde_json::Value) -> DjinnEventEnvelope {
    submitted_envelope_for_task(&f.db, &f.spike_task_id, payload).await
}

/// Production submit boundary for either the original terminal spike or its
/// repository-allocated retry task.
async fn submitted_envelope_for_task(
    db: &djinn_db::Database,
    spike_task_id: &str,
    payload: serde_json::Value,
) -> DjinnEventEnvelope {
    let context =
        djinn_slot::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    // Capture the producer's committed activity even when its independent
    // legacy structured-handoff branch declines this terminal submission.
    let _ = handle_submit_work(&serde_json::json!({ "task_id": spike_task_id, "commit_title": "deliver typed evidence", "summary": "ordinary production summary", "files_changed": [], "remaining_concerns": [], "tribunal_evidence_return_v1": payload }), spike_task_id, "fixture-session", &context).await;
    let activity = TaskRepository::new(db.clone(), EventBus::noop())
        .list_activity(spike_task_id)
        .await
        .unwrap()
        .into_iter()
        .rev()
        .find(|a| a.event_type == "tribunal_evidence_return_v1")
        .expect("submit_work committed envelope");
    let payload: serde_json::Value =
        serde_json::from_str(&activity.payload).expect("committed JSON");
    DjinnEventEnvelope::activity_logged(
        activity.task_id.as_deref(),
        &activity.event_type,
        &activity.actor_id,
        &activity.actor_role,
        &payload,
    )
}

fn normalized_invocation_identity(value: &str) -> Option<String> {
    if let Some(invocation_id) = value.strip_prefix("command:") {
        return uuid::Uuid::parse_str(invocation_id)
            .ok()
            .map(|_| "command:<generated-invocation-id>".to_owned());
    }
    uuid::Uuid::parse_str(value)
        .ok()
        .map(|_| "<generated-invocation-id>".to_owned())
}

fn normalize_generated_metadata_inner(value: &mut serde_json::Value, immutable_identity: bool) {
    match value {
        serde_json::Value::Object(map) => {
            // Preserve semantic `check_id` and every snapshot field. Only values
            // generated by independent fixtures differ across the two paths.
            for key in [
                "validation_id",
                "finding_id",
                "spike_task_id",
                "attempt_id",
                "check_result_id",
                "anchor_id",
                "invocation_id",
            ] {
                if let Some(value) = map.get_mut(key) {
                    *value = serde_json::Value::String("<generated-id>".to_owned());
                }
            }
            for key in ["payload_sha256", "raw_payload_sha256"] {
                if let Some(value) = map.get_mut(key) {
                    *value = serde_json::Value::String("<generated-payload-sha256>".to_owned());
                }
            }
            for (key, value) in map.iter_mut() {
                let is_immutable_identity = immutable_identity || key == "immutable_identity";
                if key == "locator" || is_immutable_identity {
                    normalize_generated_metadata_inner(value, is_immutable_identity);
                } else if ![
                    "validation_id",
                    "finding_id",
                    "spike_task_id",
                    "attempt_id",
                    "check_result_id",
                    "anchor_id",
                    "invocation_id",
                    "payload_sha256",
                    "raw_payload_sha256",
                ]
                .contains(&key.as_str())
                {
                    normalize_generated_metadata_inner(value, false);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for v in values {
                normalize_generated_metadata_inner(v, immutable_identity);
            }
        }
        serde_json::Value::String(value) if immutable_identity => {
            if let Some(normalized) = normalized_invocation_identity(value) {
                *value = normalized;
            }
        }
        serde_json::Value::String(value) if value.starts_with("command:") => {
            if let Some(normalized) = normalized_invocation_identity(value) {
                *value = normalized;
            }
        }
        _ => {}
    }
}
fn normalize_generated_metadata(value: &mut serde_json::Value) {
    normalize_generated_metadata_inner(value, false);
}
fn normalized(mut value: serde_json::Value) -> serde_json::Value {
    normalize_generated_metadata(&mut value);
    value
}
fn complete_snapshot(snapshot: &TypedEvidenceValidationSnapshotForTest) -> serde_json::Value {
    serde_json::json!({
        "validation_id": snapshot.validation_id,
        "payload_sha256": snapshot.payload_sha256,
        "outcome": snapshot.outcome,
        "validator_facts": snapshot.validator_facts,
        "checks": snapshot.checks,
        "check_anchors": snapshot.check_anchors,
        "findings": snapshot.findings,
        "finding_anchors": snapshot.finding_anchors,
        "failures": snapshot.failures,
        "gaps": snapshot.gaps,
        "finding_lifecycle": snapshot.finding_lifecycle,
        "transition_count": snapshot.transition_count,
    })
}
fn assert_parity(
    live: &TypedEvidenceValidationSnapshotForTest,
    cold: &TypedEvidenceValidationSnapshotForTest,
) {
    assert_eq!(
        normalized(complete_snapshot(live)),
        normalized(complete_snapshot(cold))
    );
}

async fn production_submit_live_and_cold_replay_have_complete_parity() {
    for (outcome, expected) in [
        (
            CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
            "resolved",
        ),
        (
            CanonicalTypedEvidenceReturnOutcomeForTest::Partial,
            "partial",
        ),
        (
            CanonicalTypedEvidenceReturnOutcomeForTest::Unresolved,
            "unresolved",
        ),
    ] {
        let live_fixture = fixture(outcome).await;
        let live_event = submitted_envelope(
            &live_fixture,
            serde_json::from_str(&live_fixture.delivery.return_payload).unwrap(),
        )
        .await;
        let (events, _) = tokio::sync::broadcast::channel(16);
        let mut live_actor = build_refinement_actor(
            &live_fixture.db,
            &events,
            spawn_test_pool(&live_fixture.db, 2),
        );
        live_actor.handle_event(live_event).await;
        let live = djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
            &live_fixture.db,
            &live_fixture.finding_id,
        )
        .await;
        assert_eq!(live.outcome, expected);
        assert_eq!(
            live.finding_lifecycle, "evidence_received",
            "actual outcomes remain blocking"
        );
        assert_eq!(live.transition_count, 1);
        let proposal = ProposalRepository::new(live_fixture.db.clone(), EventBus::noop())
            .get(&live_fixture.proposal_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            proposal.linked_spike_task_id.is_none() && proposal.needs_evidence_claim.is_none(),
            "only typed transaction clears compatibility state"
        );

        // A distinct database and newly constructed actor make recovery its first ingestion.
        let cold_fixture = fixture(outcome).await;
        let _committed = submitted_envelope(
            &cold_fixture,
            serde_json::from_str(&cold_fixture.delivery.return_payload).unwrap(),
        )
        .await;
        let (events, _) = tokio::sync::broadcast::channel(16);
        let mut cold_actor = build_refinement_actor(
            &cold_fixture.db,
            &events,
            spawn_test_pool(&cold_fixture.db, 2),
        );
        let replay = cold_actor.recover_terminal_linked_spike_evidence().await;
        assert_eq!(replay.len(), 1);
        assert!(!replay[0].replayed);
        let cold = djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
            &cold_fixture.db,
            &cold_fixture.finding_id,
        )
        .await;
        assert_eq!(cold.outcome, expected);
        assert_eq!(cold.finding_lifecycle, "evidence_received");
        assert_parity(&live, &cold);
    }
}

/// Stable conformance name for the complete independently persisted live/cold
/// terminal-return snapshot parity matrix above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_terminal_replay() {
    production_submit_live_and_cold_replay_have_complete_parity().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_before_resume_fault_cold_recovery_reuses_receipt_and_folds_advocate_once() {
    let f = fixture(CanonicalTypedEvidenceReturnOutcomeForTest::Resolved).await;
    let run_id = park_awaiting_evidence(&f, "commit-before-resume-ungated").await;
    let envelope = submitted_envelope(
        &f,
        serde_json::from_str(&f.delivery.return_payload).expect("canonical return payload"),
    )
    .await;
    let raw = envelope.payload["payload"].to_string();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let mut interrupted = build_refinement_actor(&f.db, &events, spawn_test_pool(&f.db, 2));

    interrupted.interrupt_after_evidence_commit_before_resume_for_test(&f.spike_task_id);
    let committed = interrupted
        .ingest_raw_tribunal_evidence_return_v1(&f.spike_task_id, &raw)
        .await
        .expect("the fault occurs only after the durable receipt commits");
    let committed_snapshot =
        djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
            &f.db,
            &f.finding_id,
        )
        .await;
    let raw_finding =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    assert!(!committed.replayed);
    assert_eq!(raw_finding.lifecycle, "evidence_received");
    assert_eq!(committed_snapshot.transition_count, 1);
    let proposal = ProposalRepository::new(f.db.clone(), EventBus::noop())
        .get(&f.proposal_id)
        .await
        .unwrap()
        .unwrap();
    assert!(proposal.linked_spike_task_id.is_none() && proposal.needs_evidence_claim.is_none());

    // Drop the interrupted actor; only a fresh actor may perform this replay.
    drop(interrupted);
    let (cold_events, _) = tokio::sync::broadcast::channel(16);
    let mut cold = build_refinement_actor(&f.db, &cold_events, spawn_test_pool(&f.db, 2));
    cold.recover_interrupted_refinements().await;
    assert_eq!(
        cold.active_refinements[&run_id].phase,
        RefinementPhase::AwaitingEvidence
    );
    let replay = cold.recover_terminal_linked_spike_evidence().await;
    assert_eq!(replay.len(), 1);
    assert!(replay[0].replayed);
    assert_eq!(replay[0].validation_id, committed.validation_id);
    assert_eq!(
        cold.active_refinements[&run_id].phase,
        RefinementPhase::AdvocateRevision
    );
    assert_eq!(
        djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
            &f.db,
            &f.finding_id,
        )
        .await,
        committed_snapshot
    );
    let duplicate = cold
        .ingest_raw_tribunal_evidence_return_v1(&f.spike_task_id, &raw)
        .await
        .unwrap();
    assert!(duplicate.replayed);
    assert_eq!(duplicate.validation_id, committed.validation_id);
    assert_eq!(
        djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
            &f.db,
            &f.finding_id,
        )
        .await,
        committed_snapshot,
        "duplicate live delivery preserves the complete receipt snapshot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_before_resume_recovery_honors_dispatch_pause_and_proposal_freeze() {
    for gate in ["global_dispatch_pause", "proposal_freeze"] {
        let f = fixture(CanonicalTypedEvidenceReturnOutcomeForTest::Resolved).await;
        let run_id = park_awaiting_evidence(&f, gate).await;
        let envelope = submitted_envelope(
            &f,
            serde_json::from_str(&f.delivery.return_payload).expect("canonical return payload"),
        )
        .await;
        let raw = envelope.payload["payload"].to_string();
        let (events, _) = tokio::sync::broadcast::channel(16);
        let mut interrupted = build_refinement_actor(&f.db, &events, spawn_test_pool(&f.db, 2));
        interrupted.interrupt_after_evidence_commit_before_resume_for_test(&f.spike_task_id);
        let committed = interrupted
            .ingest_raw_tribunal_evidence_return_v1(&f.spike_task_id, &raw)
            .await
            .unwrap();
        drop(interrupted);

        let proposals = ProposalRepository::new(f.db.clone(), EventBus::noop());
        if gate == "global_dispatch_pause" {
            DispatchPauseRepository::new(f.db.clone(), EventBus::noop())
                .pause(
                    DispatchPauseTarget::Global,
                    DispatchPause {
                        paused_by: "commit-before-resume-test".into(),
                        paused_at: ::time::OffsetDateTime::now_utc()
                            .format(&::time::format_description::well_known::Rfc3339)
                            .unwrap(),
                        reason: "prove recovery gate".into(),
                        expires_at: None,
                    },
                )
                .await
                .unwrap();
        } else {
            proposals.set_frozen(&f.proposal_id, true).await.unwrap();
        }
        let task_count = TaskRepository::new(f.db.clone(), EventBus::noop())
            .list_by_project(&f.project_id)
            .await
            .unwrap()
            .len();
        let (cold_events, _) = tokio::sync::broadcast::channel(16);
        let mut cold = build_refinement_actor(&f.db, &cold_events, spawn_test_pool(&f.db, 2));
        cold.recover_interrupted_refinements().await;
        let replay = cold.recover_terminal_linked_spike_evidence().await;
        assert!(replay[0].replayed);
        assert_eq!(replay[0].validation_id, committed.validation_id);
        assert_eq!(
            cold.active_refinements[&run_id].phase,
            RefinementPhase::AdvocateRevision
        );
        assert!(
            cold.refinement_sessions.is_empty(),
            "{gate}: Advocate was not dispatched"
        );
        assert_eq!(
            TaskRepository::new(f.db.clone(), EventBus::noop())
                .list_by_project(&f.project_id)
                .await
                .unwrap()
                .len(),
            task_count,
            "{gate}: no Advocate task was created"
        );
        let raw_finding =
            djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
                .await;
        assert_eq!(raw_finding.lifecycle, "evidence_received", "{gate}");
        assert_eq!(raw_finding.validation_count, 1, "{gate}");
        let proposal = proposals.get(&f.proposal_id).await.unwrap().unwrap();
        assert!(
            proposal.linked_spike_task_id.is_none() && proposal.needs_evidence_claim.is_none(),
            "{gate}: receipt retains compatibility cleanup"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_invalid_deliveries_fail_raw_typed_lifecycle_and_keep_link() {
    for case in [
        "malformed",
        "unsupported",
        "wrong-task",
        "wrong-attempt",
        "incomplete",
        "over-limit",
    ] {
        let f = fixture(CanonicalTypedEvidenceReturnOutcomeForTest::Resolved).await;
        let mut payload: serde_json::Value =
            serde_json::from_str(&f.delivery.return_payload).unwrap();
        match case {
            "malformed" => payload["checks"] = serde_json::json!("not an array"),
            "unsupported" => payload["version"] = serde_json::json!("TribunalEvidenceReturnV999"),
            "wrong-task" => {
                payload["spike_task_id"] = serde_json::json!("wrong-authenticated-task")
            }
            "wrong-attempt" => payload["attempt_id"] = serde_json::json!("wrong-attempt"),
            "incomplete" => payload["checks"] = serde_json::json!([]),
            "over-limit" => payload["conclusion"] = serde_json::json!("x".repeat(8193)),
            _ => unreachable!(),
        }
        let event = submitted_envelope(&f, payload).await;
        let (events, _) = tokio::sync::broadcast::channel(16);
        let mut actor = build_refinement_actor(&f.db, &events, spawn_test_pool(&f.db, 2));
        actor.handle_event(event).await;
        let raw =
            djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
                .await;
        assert_failed_without_receipt(&raw, case);
        let proposal = ProposalRepository::new(f.db.clone(), EventBus::noop())
            .get(&f.proposal_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            proposal.linked_spike_task_id.as_deref(),
            Some(f.spike_task_id.as_str()),
            "{case} retains compatibility link"
        );
        assert!(proposal.needs_evidence_claim.is_some());
    }
}

fn assert_failed_without_receipt(raw: &TypedEvidenceFindingSnapshotForTest, case: &str) {
    assert_eq!(raw.lifecycle, "failed", "{case}");
    assert_eq!(raw.validation_count, 0, "{case} has no typed receipt");
    assert_eq!(
        raw.transitions
            .last()
            .and_then(|transition| transition["to_lifecycle"].as_str()),
        Some("failed"),
        "{case} persists a raw failed transition"
    );
}

#[derive(Debug, PartialEq, Eq)]
struct ImmutableAttemptOneSnapshot {
    attempt_id: String,
    task_status: String,
    activity_history: Vec<(String, String)>,
    validation_count: i64,
}

async fn immutable_attempt_one_snapshot(f: &Fixture) -> ImmutableAttemptOneSnapshot {
    let tasks = TaskRepository::new(f.db.clone(), EventBus::noop());
    let task = tasks.get(&f.spike_task_id).await.unwrap().unwrap();
    let activity_history = tasks
        .list_activity(&f.spike_task_id)
        .await
        .unwrap()
        .into_iter()
        .map(|activity| (activity.event_type, activity.payload))
        .collect();
    ImmutableAttemptOneSnapshot {
        attempt_id: f.delivery.attempt_id.clone(),
        task_status: task.status,
        activity_history,
        validation_count:
            djinn_db::test_support::typed_evidence_validation_count_for_attempt_for_test(
                &f.db,
                &f.delivery.attempt_id,
            )
            .await,
    }
}

/// Every facet of attempt one this test can prove immutable. The fixture's
/// `attempt_1.immutable` list must name exactly this set: a token it adds has
/// no probe, and a token it drops is one this body would stop asserting.
const ATTEMPT_ONE_IMMUTABLE_FACETS: [&str; 4] =
    ["task", "payload", "validation_history", "transitions"];

/// One evolving finding: production ingress, repository retry authority, and
/// the coordinator dispatch seam prove the fixture's documented contract.
///
/// Every non-identity key of `tests/fixtures/typed_evidence_failed_retry_v1.json`
/// selects an assertion below, so corrupting any one of them reddens
/// `cargo test -p djinn-coordinator typed_evidence_failed_retry_v1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_failed_retry_v1() {
    let fixture_json: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/typed_evidence_failed_retry_v1.json"
    ))
    .expect("versioned fixture is valid JSON");
    assert_eq!(fixture_json["version"], 1);
    assert_eq!(fixture_json["fixture"], "typed_evidence_failed_retry_v1");
    let pinned = |path: [&str; 2]| -> String {
        fixture_json[path[0]][path[1]]
            .as_str()
            .unwrap_or_else(|| panic!("fixture key `{}.{}` is a string", path[0], path[1]))
            .to_owned()
    };

    let f = fixture(CanonicalTypedEvidenceReturnOutcomeForTest::Resolved).await;
    let mut malformed: serde_json::Value =
        serde_json::from_str(&f.delivery.return_payload).unwrap();
    match pinned(["attempt_1", "delivery"]).as_str() {
        "production_submit_work_malformed" => {
            malformed["checks"] = serde_json::json!("not an array")
        }
        other => panic!("unknown attempt-one delivery `{other}`"),
    }
    let attempt_one_event = submitted_envelope(&f, malformed).await;
    let (events, _) = tokio::sync::broadcast::channel(16);
    let mut first_actor = build_refinement_actor(&f.db, &events, spawn_test_pool(&f.db, 2));
    first_actor.handle_event(attempt_one_event).await;
    let before_retry =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    assert_failed_without_receipt(&before_retry, "production malformed attempt one");
    assert_eq!(
        before_retry.lifecycle,
        pinned(["attempt_1", "terminal_lifecycle"]),
        "attempt one lands in the lifecycle the fixture pins"
    );
    assert_eq!(before_retry.attempt_id, f.delivery.attempt_id);
    let immutable_attempt_one = immutable_attempt_one_snapshot(&f).await;
    let immutable_facets: Vec<String> = fixture_json["attempt_1"]["immutable"]
        .as_array()
        .expect("`attempt_1.immutable` is an array")
        .iter()
        .map(|value| value.as_str().expect("each facet is a string").to_owned())
        .collect();
    for facet in ATTEMPT_ONE_IMMUTABLE_FACETS {
        assert!(
            immutable_facets.iter().any(|listed| listed == facet),
            "`attempt_1.immutable` must claim `{facet}`; this body proves it",
        );
    }
    for facet in &immutable_facets {
        match facet.as_str() {
            "task" => assert_eq!(immutable_attempt_one.task_status, "closed"),
            "validation_history" => assert_eq!(immutable_attempt_one.validation_count, 0),
            "payload" => assert_eq!(
                immutable_attempt_one
                    .activity_history
                    .iter()
                    .filter(|(event_type, _)| event_type == "tribunal_evidence_return_v1")
                    .count(),
                1
            ),
            // Proved after the retry allocates, against `before_retry`.
            "transitions" => {}
            other => panic!("unknown attempt-one immutability facet `{other}`"),
        }
    }

    let authority_task_id = retry_authority(&f, "failed-retry-conformance").await;
    let failed_transition_id = before_retry
        .transition_ids
        .last()
        .cloned()
        .expect("failed ingress transition has durable identity");
    let proposals = ProposalRepository::new(f.db.clone(), EventBus::noop());
    match pinned(["retry", "authority"]).as_str() {
        // Every retry below goes through `ProposalRepository::retry_evidence_atomically`,
        // the single atomic repository primitive. There is no coordinator- or
        // control-plane-side retry path in this test.
        "atomic_repository_retry" => {}
        other => panic!("unknown retry authority `{other}`"),
    }
    let stale = proposals
        .retry_evidence_atomically(AtomicEvidenceRetryInput {
            finding_id: f.finding_id.clone(),
            failed_transition_id: uuid::Uuid::now_v7().to_string(),
            caller_user_id: f.user_id.clone(),
        })
        .await;
    match pinned(["retry", "stale_failure"]).as_str() {
        "rejected" => assert!(
            stale.is_err(),
            "stale failed-transition is rejected by repository authority"
        ),
        "accepted" => assert!(
            stale.is_ok(),
            "the fixture claims a stale failed-transition is accepted"
        ),
        other => panic!("unknown stale-failure disposition `{other}`"),
    }
    let first = proposals
        .retry_evidence_atomically(AtomicEvidenceRetryInput {
            finding_id: f.finding_id.clone(),
            failed_transition_id: failed_transition_id.clone(),
            caller_user_id: f.user_id.clone(),
        })
        .await
        .expect("authorized retry consumes retained failed compatibility link");
    let from_attempt = fixture_json["retry"]["from_attempt"]
        .as_i64()
        .expect("`retry.from_attempt` is a sequence number");
    let to_attempt = fixture_json["retry"]["to_attempt"]
        .as_i64()
        .expect("`retry.to_attempt` is a sequence number");
    assert_eq!(
        i64::from(first.allocation.sequence),
        to_attempt,
        "the retry allocates the attempt the fixture pins"
    );
    assert_eq!(
        to_attempt,
        from_attempt + 1,
        "a retry advances the failed attempt by exactly one"
    );
    assert_ne!(authority_task_id, first.allocation.spike_task_id);
    let after_retry =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    assert_eq!(after_retry.lifecycle, "demanded");
    assert_eq!(after_retry.attempt_id, first.allocation.attempt_id);
    if immutable_facets.iter().any(|facet| facet == "transitions") {
        assert_eq!(
            &after_retry.transitions[..before_retry.transitions.len()],
            before_retry.transitions.as_slice()
        );
    }
    assert_eq!(
        immutable_attempt_one_snapshot(&f).await,
        immutable_attempt_one
    );
    let proposal = proposals.get(&f.proposal_id).await.unwrap().unwrap();
    assert_eq!(
        proposal.linked_spike_task_id.as_deref(),
        Some(first.allocation.spike_task_id.as_str()),
        "authorized retry atomically moves the retained compatibility link"
    );

    let duplicate = proposals
        .retry_evidence_atomically(AtomicEvidenceRetryInput {
            finding_id: f.finding_id.clone(),
            failed_transition_id: failed_transition_id.clone(),
            caller_user_id: f.user_id.clone(),
        })
        .await
        .expect("duplicate retry reads allocation from repository authority");
    match pinned(["retry", "duplicate"]).as_str() {
        "stable_attempt_2_identity" => {
            assert!(duplicate.replayed);
            assert_eq!(first.allocation, duplicate.allocation);
        }
        "new_allocation" => assert_ne!(first.allocation, duplicate.allocation),
        other => panic!("unknown duplicate-retry disposition `{other}`"),
    }
    let occupied = proposals
        .retry_evidence_atomically(AtomicEvidenceRetryInput {
            finding_id: f.finding_id.clone(),
            failed_transition_id: after_retry.transition_ids.last().cloned().unwrap(),
            caller_user_id: f.user_id.clone(),
        })
        .await;
    match pinned(["retry", "occupied_slot"]).as_str() {
        "rejected" => assert!(
            matches!(
                occupied,
                Err(djinn_db::Error::InvalidTransition(message)) if message == "active_evidence_conflict"
            ),
            "occupied retry slot is rejected by repository authority"
        ),
        "accepted" => assert!(
            occupied.is_ok(),
            "the fixture claims an occupied retry slot is accepted"
        ),
        other => panic!("unknown occupied-slot disposition `{other}`"),
    }
    let open_evidence_tasks = || async {
        TaskRepository::new(f.db.clone(), EventBus::noop())
            .list_by_project(&f.project_id)
            .await
            .unwrap()
            .into_iter()
            .filter(|task| {
                task.issue_type == "spike" && matches!(task.status.as_str(), "open" | "in_progress")
            })
            .collect::<Vec<_>>()
    };
    let open_spikes = open_evidence_tasks().await;
    let expected_open = usize::try_from(
        fixture_json["terminal"]["open_active_evidence_tasks"]
            .as_u64()
            .expect("`terminal.open_active_evidence_tasks` is a count"),
    )
    .unwrap();
    assert_eq!(
        open_spikes.len(),
        expected_open,
        "the fixture pins how many evidence tasks may be open or active"
    );
    assert_eq!(open_spikes[0].id, first.allocation.spike_task_id);

    crate::evidence_dispatch_recovery::set_evidence_dispatch_test_script(
        &first.allocation.spike_task_id,
        [
            crate::evidence_dispatch_recovery::EvidenceDispatchTestOutcome::EnqueueFailed,
            crate::evidence_dispatch_recovery::EvidenceDispatchTestOutcome::Accepted,
            crate::evidence_dispatch_recovery::EvidenceDispatchTestOutcome::AlreadyActive,
        ],
        true,
    );
    let (dispatch_events, _) = tokio::sync::broadcast::channel(16);
    let mut dispatcher = build_refinement_actor(&f.db, &dispatch_events, spawn_test_pool(&f.db, 2));
    dispatcher.redrive_demanded_evidence_dispatches().await;
    let after_enqueue_failure =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    dispatcher.redrive_demanded_evidence_dispatches().await;
    let after_activation_failure =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    match pinned(["dispatch", "post_commit_activation_failure"]).as_str() {
        "retains_demanded_allocation" => {
            for (case, snapshot) in [
                ("enqueue failure", &after_enqueue_failure),
                ("activation failure", &after_activation_failure),
            ] {
                assert_eq!(snapshot.lifecycle, "demanded", "{case}");
                assert_eq!(snapshot.attempt_id, first.allocation.attempt_id, "{case}");
                assert_eq!(snapshot.transitions, after_retry.transitions, "{case}");
            }
        }
        "discards_allocation" => {
            assert_ne!(
                after_activation_failure.attempt_id,
                first.allocation.attempt_id
            )
        }
        other => panic!("unknown post-commit activation-failure disposition `{other}`"),
    }
    assert_eq!(
        immutable_attempt_one_snapshot(&f).await,
        immutable_attempt_one
    );
    drop(dispatcher);

    let (restart_events, _) = tokio::sync::broadcast::channel(16);
    let mut restarted = build_refinement_actor(&f.db, &restart_events, spawn_test_pool(&f.db, 2));
    restarted.redrive_demanded_evidence_dispatches().await;
    let after_dispatch =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    let dispatched = open_evidence_tasks().await;
    match pinned(["dispatch", "restart_redrive"]).as_str() {
        "same_task_and_attempt" => {
            assert_eq!(after_dispatch.lifecycle, "spike_active");
            assert_eq!(after_dispatch.attempt_id, first.allocation.attempt_id);
            assert_eq!(
                dispatched
                    .iter()
                    .map(|task| task.id.as_str())
                    .collect::<Vec<_>>(),
                vec![first.allocation.spike_task_id.as_str()],
                "the restart re-drives the exact allocated task, never a replacement",
            );
        }
        "new_task" => assert!(
            !dispatched
                .iter()
                .any(|task| task.id == first.allocation.spike_task_id),
            "the fixture claims the restart re-drives onto a replacement task"
        ),
        other => panic!("unknown restart-redrive disposition `{other}`"),
    }
    if immutable_facets.iter().any(|facet| facet == "transitions") {
        assert_eq!(
            &after_dispatch.transitions[..before_retry.transitions.len()],
            before_retry.transitions.as_slice()
        );
    }
    assert_eq!(
        immutable_attempt_one_snapshot(&f).await,
        immutable_attempt_one
    );

    let mut valid: serde_json::Value = serde_json::from_str(&f.delivery.return_payload).unwrap();
    valid["attempt_id"] = serde_json::json!(first.allocation.attempt_id);
    valid["spike_task_id"] = serde_json::json!(first.allocation.spike_task_id);
    let valid_event =
        submitted_envelope_for_task(&f.db, &first.allocation.spike_task_id, valid).await;
    // `submit_work` durably emits the typed activity but deliberately does not
    // close its task. The coordinator's production ingress accepts terminal
    // evidence only from a closed spike, so finish this exact allocated retry
    // through the repository status transition before consuming its committed
    // envelope. This is the production terminal task action, not fixture repair.
    TaskRepository::new(f.db.clone(), EventBus::noop())
        .set_status_with_reason(&first.allocation.spike_task_id, "closed", Some("completed"))
        .await
        .expect("terminally close the exact allocated retry task");
    restarted.handle_event(valid_event).await;
    let receipt = djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
        &f.db,
        &f.finding_id,
    )
    .await;
    assert_eq!(
        receipt.finding_lifecycle,
        pinned(["terminal", "attempt_2_durable_receipt"]),
        "the retried attempt lands the durable receipt the fixture pins"
    );
    assert_eq!(receipt.transition_count, 1);
    assert_eq!(
        immutable_attempt_one_snapshot(&f).await,
        immutable_attempt_one
    );
    let attempt_one_status = TaskRepository::new(f.db.clone(), EventBus::noop())
        .get(&f.spike_task_id)
        .await
        .unwrap()
        .unwrap()
        .status;
    assert_eq!(
        attempt_one_status == "closed",
        fixture_json["terminal"]["attempt_1_task_never_reopens"]
            .as_bool()
            .expect("`terminal.attempt_1_task_never_reopens` is a boolean"),
        "attempt one's terminal task status must match the fixture claim; observed {attempt_one_status}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_envelope_terminal_activity_never_creates_typed_receipt() {
    let f = fixture(CanonicalTypedEvidenceReturnOutcomeForTest::Resolved).await;
    let tasks = TaskRepository::new(f.db.clone(), EventBus::noop());
    // The fixture has a payload-free terminal close; each remaining ordinary path
    // is explicitly delivered through handle_event and cannot create a receipt.
    for (kind, payload) in [
        ("comment", serde_json::json!({"body":"comment"})),
        ("task_closed", serde_json::json!({"summary":"close text"})),
        (
            "work_submitted",
            serde_json::json!({"commit_title":"ordinary work", "summary":"ordinary summary", "files_changed":[], "remaining_concerns":[]}),
        ),
        ("findings", serde_json::json!({"body":"findings prose"})),
        ("memory_link", serde_json::json!({"memory":"[[evidence]]"})),
    ] {
        tasks
            .log_activity(
                Some(&f.spike_task_id),
                "worker",
                "worker",
                kind,
                &payload.to_string(),
            )
            .await
            .unwrap();
    }
    let (events, _) = tokio::sync::broadcast::channel(16);
    let mut actor = build_refinement_actor(&f.db, &events, spawn_test_pool(&f.db, 2));
    for activity in tasks.list_activity(&f.spike_task_id).await.unwrap() {
        let payload: serde_json::Value = serde_json::from_str(&activity.payload).unwrap();
        actor
            .handle_event(DjinnEventEnvelope::activity_logged(
                activity.task_id.as_deref(),
                &activity.event_type,
                &activity.actor_id,
                &activity.actor_role,
                &payload,
            ))
            .await;
    }
    assert!(
        TypedEvidenceRepository::new(f.db.clone())
            .terminal_return_v1_deliveries_for_task(&f.spike_task_id)
            .await
            .unwrap()
            .is_empty(),
        "no typed receipt exists"
    );
    let proposal = ProposalRepository::new(f.db.clone(), EventBus::noop())
        .get(&f.proposal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        proposal.linked_spike_task_id.as_deref(),
        Some(f.spike_task_id.as_str())
    );
    assert!(proposal.needs_evidence_claim.is_some());
    let raw =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    assert_eq!(raw.lifecycle, "spike_active");
    assert_eq!(raw.validation_count, 0, "no typed receipt was persisted");
    assert!(
        raw.transitions.iter().all(|transition| {
            !matches!(
                transition["to_lifecycle"].as_str(),
                Some("evidence_received" | "failed")
            )
        }),
        "ordinary closure produces no typed terminal transition"
    );

    // Recovery over the same payload-free terminal state must not infer receipt.
    drop(actor);
    let (events, _) = tokio::sync::broadcast::channel(16);
    let mut cold_actor = build_refinement_actor(&f.db, &events, spawn_test_pool(&f.db, 2));
    assert!(
        cold_actor
            .recover_terminal_linked_spike_evidence()
            .await
            .is_empty(),
        "cold recovery cannot manufacture a typed receipt"
    );
    let raw =
        djinn_db::test_support::typed_evidence_finding_snapshot_for_test(&f.db, &f.finding_id)
            .await;
    assert_eq!(raw.lifecycle, "spike_active");
    assert_eq!(
        raw.validation_count, 0,
        "cold recovery persisted no receipt"
    );
    assert!(
        raw.transitions.iter().all(|transition| {
            !matches!(
                transition["to_lifecycle"].as_str(),
                Some("evidence_received" | "failed")
            )
        }),
        "cold recovery produces no typed terminal transition"
    );
}
