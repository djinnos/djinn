// djinn:allow-oversize
//! Typed-evidence ingress coverage deliberately crosses the production Slot
//! activity boundary before exercising the coordinator's live and cold paths.

use crate::refinement_dispatch::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, spawn_test_pool,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::NeedsEvidenceClaim;
use djinn_db::test_support::{
    CanonicalTypedEvidenceReturnOutcomeForTest, TypedEvidenceFindingSnapshotForTest,
    TypedEvidenceIngressFixtureForTest, TypedEvidenceValidationSnapshotForTest,
};
use djinn_db::{
    EffectiveCreatorProvenance, ProposalRepository, TaskRepository, TypedEvidenceRepository,
};
use djinn_slot::finalize_handlers::handle_submit_work;
use tokio_util::sync::CancellationToken;

struct Fixture {
    db: djinn_db::Database,
    proposal_id: String,
    spike_task_id: String,
    finding_id: String,
    delivery: TypedEvidenceIngressFixtureForTest,
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
        proposal_id: refinement.proposal_id,
        spike_task_id,
        finding_id: delivery.finding_id.clone(),
        delivery,
    }
}

/// Real Slot submission followed by capture of its committed durable payload.
async fn submitted_envelope(f: &Fixture, payload: serde_json::Value) -> DjinnEventEnvelope {
    let context =
        djinn_slot::test_helpers::agent_context_from_db(f.db.clone(), CancellationToken::new());
    // Capture the producer's committed activity even when its independent
    // legacy structured-handoff branch declines this terminal submission.
    let _ = handle_submit_work(&serde_json::json!({ "task_id": f.spike_task_id, "commit_title": "deliver typed evidence", "summary": "ordinary production summary", "files_changed": [], "remaining_concerns": [], "tribunal_evidence_return_v1": payload }), &f.spike_task_id, "fixture-session", &context).await;
    let activity = TaskRepository::new(f.db.clone(), EventBus::noop())
        .list_activity(&f.spike_task_id)
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

fn normalize_generated_metadata(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // Preserve semantic `check_id`; only independent fixture identity
            // and derived raw-payload hash fields may differ across paths.
            for key in [
                "validation_id",
                "payload_sha256",
                "raw_payload_sha256",
                "finding_id",
                "spike_task_id",
                "attempt_id",
                "check_result_id",
                "anchor_id",
                "invocation_id",
            ] {
                map.remove(key);
            }
            for v in map.values_mut() {
                normalize_generated_metadata(v);
            }
        }
        serde_json::Value::Array(values) => {
            for v in values {
                normalize_generated_metadata(v);
            }
        }
        _ => {}
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
