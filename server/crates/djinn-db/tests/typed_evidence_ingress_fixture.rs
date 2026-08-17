use djinn_core::{events::EventBus, models::NeedsEvidenceClaim};
use djinn_db::{
    Database, ProposalRepository, TypedEvidenceRepository,
    test_support::{
        CanonicalTypedEvidenceReturnOutcomeForTest, UsageTestTaskSeed,
        seed_canonical_typed_evidence_ingress_fixture_for_test, seed_project, seed_task_row,
        typed_evidence_validation_snapshot_for_test,
    },
};
use serde_json::json;

async fn fixture_parent_rows(db: &Database) -> (String, String) {
    let project_id = uuid::Uuid::now_v7().to_string();
    seed_project(db, &project_id, &format!("fixture-{project_id}")).await;
    let spike_task_id = seed_task_row(
        db,
        UsageTestTaskSeed {
            project_id: &project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let proposal_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'fixture','','markdown','[]','draft',1)")
        .bind(&proposal_id)
        .bind(proposal_id.replace('-', ""))
        .execute(db.pool())
        .await
        .unwrap();
    (proposal_id, spike_task_id)
}

#[tokio::test]
async fn canonical_fixture_reuses_exact_repository_authority() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let (proposal_id, spike_task_id) = fixture_parent_rows(&db).await;
    let claim = NeedsEvidenceClaim {
        question: "Can the canonical fixture preserve authority?".into(),
        target_subsystem: "typed evidence test support".into(),
        spec_unknown_anchor: "existing finding and attempt".into(),
        insufficient_in_session_research: "requires persisted hydration".into(),
        expected_findings: "immutable command evidence".into(),
        created_by_task_id: spike_task_id.clone(),
        round: 1,
        against_revision_seq: 1,
    };
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_structured_needs_evidence_spike(&proposal_id, &spike_task_id, &claim)
        .await
        .unwrap();
    let authority: (String, String) = sqlx::query_as(
        "SELECT f.id,a.id FROM typed_evidence_findings f \
         JOIN typed_evidence_attempts a ON a.finding_id=f.id \
         WHERE f.proposal_id=$1 AND a.spike_task_id=$2",
    )
    .bind(&proposal_id)
    .bind(&spike_task_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    let fixture = seed_canonical_typed_evidence_ingress_fixture_for_test(
        &db,
        &proposal_id,
        &spike_task_id,
        "preseeded",
        CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
    )
    .await;
    assert_eq!(fixture.finding_id, authority.0);
    assert_eq!(fixture.attempt_id, authority.1);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM typed_evidence_findings WHERE proposal_id=$1), \
                (SELECT count(*) FROM typed_evidence_attempts WHERE finding_id=$2)",
    )
    .bind(&proposal_id)
    .bind(&fixture.finding_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(counts, (1, 1));

    let result = TypedEvidenceRepository::new(db.clone())
        .submit_return_v1(fixture.return_payload.as_bytes())
        .await
        .unwrap();
    let snapshot = typed_evidence_validation_snapshot_for_test(&db, &result.validation_id).await;
    assert_eq!(snapshot.outcome, "resolved");
    assert_eq!(snapshot.check_anchors[0]["health"], "healthy");
    assert_eq!(snapshot.check_anchors[0]["method_compatible"], true);
}

#[tokio::test]
async fn canonical_typed_evidence_ingress_fixtures_persist_all_outcomes_and_hydration() {
    for (expected, outcome) in [
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
        let db = Database::ephemeral().await.unwrap();
        db.ensure_initialized().await.unwrap();
        let (proposal_id, spike_task_id) = fixture_parent_rows(&db).await;
        let fixture = seed_canonical_typed_evidence_ingress_fixture_for_test(
            &db,
            &proposal_id,
            &spike_task_id,
            "canonical",
            expected,
        )
        .await;
        let mut payload: serde_json::Value = serde_json::from_str(&fixture.return_payload).unwrap();
        // Exercise the return-finding provenance branch using the fixture's
        // immutable command anchor, rather than inventing a caller-owned one.
        if outcome != "unresolved" {
            payload["findings"] = json!([{
                "check_id": payload["checks"][0]["check_id"].clone(),
                "conclusion": "canonical finding provenance",
                "anchors": payload["checks"][0]["anchors"].clone(),
            }]);
        }
        let result = TypedEvidenceRepository::new(db.clone())
            .submit_return_v1(&serde_json::to_vec(&payload).unwrap())
            .await
            .unwrap();
        assert_eq!(format!("{:?}", result.outcome).to_lowercase(), outcome);

        let snapshot =
            typed_evidence_validation_snapshot_for_test(&db, &result.validation_id).await;
        assert_eq!(snapshot.validation_id, result.validation_id);
        assert!(!snapshot.payload_sha256.is_empty());
        assert_eq!(snapshot.outcome, outcome);
        assert_eq!(
            snapshot.validator_facts["validator_version"],
            "TribunalEvidenceReturnV1"
        );
        assert_eq!(snapshot.finding_lifecycle, "evidence_received");
        assert_eq!(snapshot.transition_count, 1);

        match outcome {
            "resolved" => {
                assert_eq!(snapshot.checks.len(), 1);
                assert_eq!(snapshot.checks[0]["status"], "passed");
                assert_eq!(snapshot.checks[0]["invocation_usable"], true);
                assert_eq!(snapshot.check_anchors.len(), 1);
                assert_eq!(snapshot.check_anchors[0]["health"], "healthy");
                assert_eq!(snapshot.check_anchors[0]["method_compatible"], true);
                assert_eq!(
                    snapshot.check_anchors[0]["detail"],
                    "exact successful command invocation"
                );
                assert_eq!(
                    snapshot.check_anchors[0]["immutable_identity"]["invocation_id"],
                    snapshot.checks[0]["invocation_id"]
                );
                assert_eq!(snapshot.findings.len(), 1);
                assert!(
                    !snapshot.findings[0]["finding_id"]
                        .as_str()
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(snapshot.findings[0]["usable"], true);
                assert!(
                    !snapshot.check_anchors[0]["anchor_id"]
                        .as_str()
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    !snapshot.finding_anchors[0]["anchor_id"]
                        .as_str()
                        .unwrap()
                        .is_empty()
                );
                assert_ne!(
                    snapshot.finding_anchors[0]["anchor_id"],
                    snapshot.check_anchors[0]["anchor_id"]
                );
                for field in [
                    "check_id",
                    "method",
                    "locator",
                    "health",
                    "detail",
                    "immutable_identity",
                    "method_compatible",
                ] {
                    assert_eq!(
                        snapshot.finding_anchors[0][field], snapshot.check_anchors[0][field],
                        "finding anchor must preserve hydrated {field} provenance"
                    );
                }
            }
            "partial" => {
                assert_eq!(snapshot.checks.len(), 2);
                assert_eq!(snapshot.failures.len(), 1);
                assert_eq!(snapshot.failures[0]["detail"], "canonical partial failure");
                assert_eq!(snapshot.check_anchors[0]["health"], "healthy");
                assert_eq!(snapshot.finding_anchors[0]["method_compatible"], true);
            }
            "unresolved" => {
                assert_eq!(snapshot.checks[0]["status"], "not_run");
                assert_eq!(snapshot.checks[0]["detail"], "canonical unresolved gap");
                assert!(snapshot.check_anchors.is_empty());
                assert!(snapshot.findings.is_empty());
                assert!(snapshot.finding_anchors.is_empty());
                assert_eq!(snapshot.gaps[0]["detail"], "canonical unresolved gap");
            }
            _ => unreachable!(),
        }
    }
}
