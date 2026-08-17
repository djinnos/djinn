//! Contract for the typed unresolved-finding projection and its legacy parity
//! probe.
//!
//! The projection is the structural gate's only new input, so every assertion
//! here is on persisted repository state reached through production writers.
//! Lifecycle states are produced by `set_structured_needs_evidence_spike`,
//! `submit_return_v1_for_task`, and `dispose_in_transaction` rather than by
//! writing a lifecycle string, so a row cannot be labelled into a state the
//! repository would refuse to reach.

use djinn_core::{
    events::EventBus,
    models::{NeedsEvidenceClaim, TribunalEvidenceLifecycle, TribunalEvidenceOutcome},
};
use djinn_db::{
    AdmitRefinementRunRequest, Database, DemandTypedEvidenceInput, ProposalCreateInput,
    ProposalRepository, ProposalUpdateInput, RefinementAdmissionOutcome, RefinementAdmissionSource,
    TypedEvidenceParityMismatchReason, TypedEvidenceParityProbe, TypedEvidenceRepository,
    UnresolvedTypedEvidenceProjection, legacy_demand_hash,
    test_support::{
        CanonicalTypedEvidenceReturnOutcomeForTest, UsageTestTaskSeed,
        dispose_typed_evidence_validation_for_test, materialize_judge_authority_for_test,
        overwrite_legacy_evidence_authority_for_test,
        seed_canonical_typed_evidence_ingress_fixture_for_test, seed_project, seed_task_row,
    },
};

struct Fixture {
    db: Database,
    proposals: ProposalRepository,
    project_id: String,
    proposal_id: String,
    spike_task_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let db = Database::ephemeral().await.unwrap();
        db.ensure_initialized().await.unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        seed_project(&db, &project_id, &format!("fixture-{project_id}")).await;
        let spike_task_id = seed_task_row(
            &db,
            UsageTestTaskSeed {
                project_id: &project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = proposals
            .create(ProposalCreateInput {
                title: "typed evidence projection fixture",
                body: "Fixture body for the unresolved typed evidence projection.",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        Self {
            db,
            proposals,
            project_id,
            proposal_id: proposal.id,
            spike_task_id,
        }
    }

    fn typed(&self) -> TypedEvidenceRepository {
        TypedEvidenceRepository::new(self.db.clone())
    }

    fn claim(&self) -> NeedsEvidenceClaim {
        NeedsEvidenceClaim {
            question: "Does the unresolved projection survive later revisions?".into(),
            target_subsystem: "typed evidence projection".into(),
            spec_unknown_anchor: "gate diagnostics".into(),
            insufficient_in_session_research: "requires persisted lifecycle".into(),
            expected_findings: "finding id, claim, lifecycle, originating revision".into(),
            created_by_task_id: self.spike_task_id.clone(),
            round: 1,
            against_revision_seq: 1,
        }
    }

    async fn projection(&self) -> Option<UnresolvedTypedEvidenceProjection> {
        self.typed()
            .unresolved_projection(&self.proposal_id)
            .await
            .unwrap()
    }

    async fn probe(&self) -> TypedEvidenceParityProbe {
        self.typed()
            .legacy_parity_probe(&self.proposal_id)
            .await
            .unwrap()
    }

    /// Reach `demanded`: a typed demand with no spike allocated yet.
    async fn demand(&self) -> String {
        let claim = serde_json::to_value(self.claim()).unwrap();
        let finding_id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.db.pool().begin().await.unwrap();
        TypedEvidenceRepository::demand_and_set_legacy_in_transaction(
            &mut tx,
            DemandTypedEvidenceInput {
                finding_id: finding_id.clone(),
                proposal_id: self.proposal_id.clone(),
                demand_hash: legacy_demand_hash(&claim, None),
                claim,
                demanded_revision_seq: 1,
                judge_task_id: self.spike_task_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        finding_id
    }

    /// Reach `spike_active` through the production structured demand writer.
    async fn spike_active(&self) -> String {
        self.proposals
            .set_structured_needs_evidence_spike(
                &self.proposal_id,
                &self.spike_task_id,
                &self.claim(),
            )
            .await
            .unwrap();
        self.projection().await.unwrap().finding_id
    }

    /// Reach `evidence_received` by submitting a canonical durable return.
    async fn evidence_received(
        &self,
        expected: CanonicalTypedEvidenceReturnOutcomeForTest,
    ) -> (String, String) {
        let finding_id = self.spike_active().await;
        let fixture = seed_canonical_typed_evidence_ingress_fixture_for_test(
            &self.db,
            &self.proposal_id,
            &self.spike_task_id,
            "projection",
            expected,
        )
        .await;
        assert_eq!(fixture.finding_id, finding_id);
        close_spike_task(&self.db, &self.spike_task_id).await;
        let result = self
            .typed()
            .submit_return_v1_for_task(&self.spike_task_id, fixture.return_payload.as_bytes())
            .await
            .unwrap();
        (finding_id, result.validation_id)
    }

    /// Reach `failed` through the ingress rejection path: a payload the
    /// validator refuses records an append-only failure fact for the attempt.
    async fn failed(&self) -> String {
        let finding_id = self.spike_active().await;
        let fixture = seed_canonical_typed_evidence_ingress_fixture_for_test(
            &self.db,
            &self.proposal_id,
            &self.spike_task_id,
            "projection",
            CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
        )
        .await;
        close_spike_task(&self.db, &self.spike_task_id).await;
        let mut payload: serde_json::Value = serde_json::from_str(&fixture.return_payload).unwrap();
        // A rejected conclusion is validated before any normalized row is
        // written, so this exercises the real rejection branch.
        payload["conclusion"] = serde_json::json!("");
        let error = self
            .typed()
            .submit_return_v1_for_task(
                &self.spike_task_id,
                serde_json::to_string(&payload).unwrap().as_bytes(),
            )
            .await
            .expect_err("a malformed return must be rejected");
        assert!(
            format!("{error}").contains("missing_identity"),
            "unexpected rejection: {error}"
        );
        finding_id
    }

    /// Materialize an active Judge and fold the received evidence terminally.
    async fn dispose(&self, validation_id: &str, disposition: TribunalEvidenceLifecycle) {
        self.proposals
            .record_refinement_lifecycle(&self.proposal_id, "refinement_start", None)
            .await
            .unwrap();
        let (run_id, generation) = match self
            .proposals
            .reap_and_admit(AdmitRefinementRunRequest {
                proposal_id: self.proposal_id.clone(),
                idempotency_key: format!("projection/{}/{disposition:?}", self.proposal_id),
                source: RefinementAdmissionSource::Demand {
                    demand_id: format!("projection/{}/{disposition:?}", self.proposal_id),
                },
                heartbeat_grace_millis: 60_000,
            })
            .await
            .unwrap()
        {
            RefinementAdmissionOutcome::Admitted {
                run_id, generation, ..
            }
            | RefinementAdmissionOutcome::Existing {
                run_id, generation, ..
            } => (run_id, generation),
        };
        let judge_task_id = seed_task_row(
            &self.db,
            UsageTestTaskSeed {
                project_id: &self.project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        materialize_judge_authority_for_test(
            &self.db,
            &judge_task_id,
            &run_id,
            i64::from(generation),
        )
        .await;
        let result = dispose_typed_evidence_validation_for_test(
            &self.db,
            validation_id,
            &judge_task_id,
            disposition,
        )
        .await;
        assert_eq!(result.finding_lifecycle, disposition);
    }

    /// Append two further spec revisions after the demand was raised.
    async fn advance_two_revisions(&self) {
        for seq in 2..=3 {
            let proposal = self
                .proposals
                .get(&self.proposal_id)
                .await
                .unwrap()
                .unwrap();
            let updated = self
                .proposals
                .update(
                    &self.proposal_id,
                    ProposalUpdateInput {
                        title: &proposal.title,
                        body: &format!("Fixture body revised for revision {seq}."),
                        acceptance_criteria: &proposal.acceptance_criteria,
                        status: &proposal.status,
                        superseded_by: None,
                        body_format: None,
                        event_metadata: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(updated.latest_revision_seq, seq);
        }
    }
}

/// The durable return path authenticates a closed spike task.
async fn close_spike_task(db: &Database, spike_task_id: &str) {
    sqlx::query("UPDATE tasks SET status='closed', close_reason='completed' WHERE id=$1")
        .bind(spike_task_id)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Named so `cargo test -p djinn-db typed_evidence_unresolved_projection`
/// selects every case in this file.
mod typed_evidence_unresolved_projection {
    use super::*;

    #[tokio::test]
    async fn projection_returns_the_finding_for_every_blocking_lifecycle() {
        // demanded
        let fixture = Fixture::new().await;
        let finding_id = fixture.demand().await;
        let projection = fixture.projection().await.expect("demanded must project");
        assert_eq!(projection.finding_id, finding_id);
        assert_eq!(projection.lifecycle, TribunalEvidenceLifecycle::Demanded);
        assert_eq!(projection.demanded_revision_seq, 1);
        assert_eq!(projection.attempt_seq, None);
        assert_eq!(projection.evidence_outcome, None);
        assert_eq!(projection.folding_revision, None);
        assert_eq!(
            projection.claim["question"],
            serde_json::json!("Does the unresolved projection survive later revisions?")
        );

        // spike_active
        let fixture = Fixture::new().await;
        let finding_id = fixture.spike_active().await;
        let projection = fixture
            .projection()
            .await
            .expect("spike_active must project");
        assert_eq!(projection.finding_id, finding_id);
        assert_eq!(projection.lifecycle, TribunalEvidenceLifecycle::SpikeActive);
        assert_eq!(projection.attempt_seq, Some(1));

        // evidence_received, for each of the three validated outcomes
        for (expected, outcome) in [
            (
                CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
                TribunalEvidenceOutcome::Resolved,
            ),
            (
                CanonicalTypedEvidenceReturnOutcomeForTest::Partial,
                TribunalEvidenceOutcome::Partial,
            ),
            (
                CanonicalTypedEvidenceReturnOutcomeForTest::Unresolved,
                TribunalEvidenceOutcome::Unresolved,
            ),
        ] {
            let fixture = Fixture::new().await;
            let (finding_id, _) = fixture.evidence_received(expected).await;
            let projection = fixture
                .projection()
                .await
                .unwrap_or_else(|| panic!("evidence_received/{outcome:?} must project"));
            assert_eq!(projection.finding_id, finding_id);
            assert_eq!(
                projection.lifecycle,
                TribunalEvidenceLifecycle::EvidenceReceived
            );
            assert_eq!(projection.evidence_outcome, Some(outcome));
            assert_eq!(projection.attempt_seq, Some(1));
            // A resolved return has neither a failure nor a gap; the other two do.
            assert_eq!(
                projection.failure_detail.is_some(),
                outcome != TribunalEvidenceOutcome::Resolved,
                "failure detail presence must follow the validated outcome"
            );
        }

        // failed
        let fixture = Fixture::new().await;
        let finding_id = fixture.failed().await;
        let projection = fixture.projection().await.expect("failed must project");
        assert_eq!(projection.finding_id, finding_id);
        assert_eq!(projection.lifecycle, TribunalEvidenceLifecycle::Failed);
        assert_eq!(
            projection.failure_detail.as_deref(),
            Some("missing_identity"),
            "a failed finding carries its persisted ingress validation error"
        );
    }

    #[tokio::test]
    async fn projection_is_absent_for_both_terminal_lifecycles() {
        for disposition in [
            TribunalEvidenceLifecycle::Resolved,
            TribunalEvidenceLifecycle::Withdrawn,
        ] {
            let fixture = Fixture::new().await;
            let (finding_id, validation_id) = fixture
                .evidence_received(CanonicalTypedEvidenceReturnOutcomeForTest::Resolved)
                .await;
            // Before the disposition the same finding is projected. This is the
            // mutation fence for the lifecycle predicate: if the predicate is
            // deleted the assertion below returns this exact id instead of `None`.
            assert_eq!(
                fixture.projection().await.map(|p| p.finding_id).as_deref(),
                Some(finding_id.as_str())
            );
            fixture.dispose(&validation_id, disposition).await;
            assert_eq!(
                fixture.projection().await.map(|p| p.finding_id),
                None,
                "{disposition:?} must not project, and must not project as {finding_id}"
            );
        }
    }

    #[tokio::test]
    async fn later_revisions_do_not_change_the_projection() {
        let fixture = Fixture::new().await;
        let finding_id = fixture.spike_active().await;
        let before = fixture.projection().await.expect("demand must project");
        assert_eq!(before.finding_id, finding_id);
        assert_eq!(before.demanded_revision_seq, 1);

        fixture.advance_two_revisions().await;
        assert_eq!(
            fixture
                .proposals
                .get(&fixture.proposal_id)
                .await
                .unwrap()
                .unwrap()
                .latest_revision_seq,
            3,
            "the fixture must actually advance the proposal head to N+2"
        );

        let after = fixture
            .projection()
            .await
            .expect("the demand still blocks at revision N+2");
        assert_eq!(
            after, before,
            "demanded_revision_seq is provenance; the projection is selected proposal-wide"
        );
        assert_eq!(after.demanded_revision_seq, 1);
    }

    #[tokio::test]
    async fn parity_probe_agrees_with_matching_authority() {
        let fixture = Fixture::new().await;
        assert_eq!(fixture.probe().await, TypedEvidenceParityProbe::Agreed);
        fixture.spike_active().await;
        assert_eq!(
            fixture.probe().await,
            TypedEvidenceParityProbe::Agreed,
            "a dual-written demand must not read as a mismatch"
        );
    }

    #[tokio::test]
    async fn parity_probe_distinguishes_the_two_drift_directions() {
        // Legacy ahead: a compatibility link with no typed finding behind it.
        let fixture = Fixture::new().await;
        overwrite_legacy_evidence_authority_for_test(
            &fixture.db,
            &fixture.proposal_id,
            Some(&fixture.spike_task_id),
            Some(&serde_json::to_value(fixture.claim()).unwrap()),
        )
        .await;
        assert_eq!(fixture.projection().await, None);
        assert_eq!(
            fixture.probe().await,
            TypedEvidenceParityProbe::Mismatch(
                TypedEvidenceParityMismatchReason::LegacyAuthorityWithoutTypedFinding
            )
        );

        // Typed ahead: an unresolved pre-return finding whose legacy link is gone.
        let fixture = Fixture::new().await;
        let finding_id = fixture.spike_active().await;
        overwrite_legacy_evidence_authority_for_test(&fixture.db, &fixture.proposal_id, None, None)
            .await;
        assert_eq!(
            fixture.projection().await.map(|p| p.finding_id).as_deref(),
            Some(finding_id.as_str()),
            "the typed finding must still be unresolved for this to be drift"
        );
        assert_eq!(
            fixture.probe().await,
            TypedEvidenceParityProbe::Mismatch(
                TypedEvidenceParityMismatchReason::TypedFindingWithoutLegacyAuthority
            )
        );
    }

    #[tokio::test]
    async fn parity_mismatch_reason_codes_are_distinct_and_typed() {
        let codes = [
            TypedEvidenceParityMismatchReason::LegacyAuthorityWithoutTypedFinding,
            TypedEvidenceParityMismatchReason::TypedFindingWithoutLegacyAuthority,
            TypedEvidenceParityMismatchReason::LegacyBindingDisagrees,
            TypedEvidenceParityMismatchReason::ProposalMissing,
        ]
        .map(TypedEvidenceParityMismatchReason::as_str);
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "reason codes must be distinct");
        assert_eq!(
            TypedEvidenceParityProbe::Agreed.mismatch_reason(),
            None,
            "agreement carries no reason code"
        );
    }

    #[tokio::test]
    async fn parity_probe_rejects_a_repointed_legacy_link() {
        let fixture = Fixture::new().await;
        fixture.spike_active().await;
        let other_task_id = seed_task_row(
            &fixture.db,
            UsageTestTaskSeed {
                project_id: &fixture.project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        overwrite_legacy_evidence_authority_for_test(
            &fixture.db,
            &fixture.proposal_id,
            Some(&other_task_id),
            Some(&serde_json::to_value(fixture.claim()).unwrap()),
        )
        .await;
        assert_eq!(
            fixture.probe().await,
            TypedEvidenceParityProbe::Mismatch(
                TypedEvidenceParityMismatchReason::LegacyBindingDisagrees
            )
        );
    }

    #[tokio::test]
    async fn parity_probe_reports_a_missing_proposal() {
        let db = Database::ephemeral().await.unwrap();
        db.ensure_initialized().await.unwrap();
        assert_eq!(
            TypedEvidenceRepository::new(db)
                .legacy_parity_probe(&uuid::Uuid::now_v7().to_string())
                .await
                .unwrap(),
            TypedEvidenceParityProbe::Mismatch(TypedEvidenceParityMismatchReason::ProposalMissing)
        );
    }
}
