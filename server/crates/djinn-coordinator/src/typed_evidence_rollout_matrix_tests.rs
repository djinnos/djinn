// djinn:allow-oversize
//! Executable six-phase typed-evidence rollout and reverse-rollback matrix.
//!
//! The fixture beside this module is a **program**, not a verdict. Each phase
//! names the old/new writer and old/new reader operations the executor has to
//! perform; the executor appends a program entry only after it has performed
//! that operation against real coordinator and repository state and its
//! persisted-state assertions have passed. Deleting the body of
//! `backfill_active_legacy_evidence` or `dual_read_legacy_parity` makes this
//! test red, because every phase asserts on rows those functions write and on
//! the projections they gate.
//!
//! Historical deployment shapes (a legacy-only writer, a mixed-version
//! operator repointing the compatibility columns) are materialized by
//! `djinn_db::test_support`, so no raw SQL crosses into this crate.

use crate::evidence_dispatch_recovery::{
    EvidenceDispatchTestOutcome, evidence_dispatch_test_count, set_evidence_dispatch_test_script,
};
use crate::evidence_lifecycle_state::EvidenceLifecycleState;
use crate::refinement::RefinementPhase;
use crate::refinement_dispatch::refinement_cap_tests::{
    build_refinement_actor, seed_refinement_fixture, seed_refinement_state, spawn_test_pool,
};
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{NeedsEvidenceClaim, TribunalEvidenceLifecycle};
use djinn_db::test_support::{
    EvidenceDispatchRecoverySnapshotForTest, TypedEvidenceRolloutSnapshotForTest,
    evidence_dispatch_recovery_snapshot_for_test, evidence_plan_invocation_availability_for_test,
    materialize_evidence_dispatch_recovery_fixture_for_test,
    overwrite_legacy_evidence_authority_for_test,
    seed_canonical_typed_evidence_ingress_fixture_for_test, task_row_count_for_test,
    typed_evidence_rollout_snapshot_for_test, typed_evidence_validation_snapshot_for_test,
    write_legacy_only_evidence_authority_for_test,
};
use djinn_db::{
    Database, EffectiveCreatorProvenance, ProposalRepository, TaskRepository,
    TypedEvidenceLifecycleProjection, TypedEvidenceRepository, legacy_demand_hash,
};
use serde::Deserialize;
use serde_json::Value;

const ROLLOUT_MATRIX: &str = include_str!("../tests/fixtures/typed_evidence_rollout_matrix.json");

/// The six deployment phases every rollout row must drive, in rollout order.
const REQUIRED_PHASES: [&str; 6] = [
    "legacy_only",
    "dual_write_legacy_read",
    "dual_write_dual_read",
    "typed_write_dual_read",
    "typed_only",
    "reverse_rollback",
];

#[derive(Deserialize)]
struct RolloutFixture {
    fixture: String,
    phases: Vec<RolloutPhaseRow>,
}

#[derive(Deserialize)]
struct RolloutPhaseRow {
    phase: String,
    writers: Vec<String>,
    readers: Vec<String>,
    program: Vec<String>,
}

/// Ledger of operations that actually ran. An entry is appended by the code
/// that just performed the operation and asserted its persisted effect, so the
/// ledger cannot run ahead of the executor.
#[derive(Default)]
struct Executed {
    ops: Vec<String>,
    writers: Vec<String>,
    readers: Vec<String>,
}

impl Executed {
    fn op(&mut self, name: &str) {
        self.ops.push(name.to_owned());
    }
    fn wrote(&mut self, role: &str) {
        if !self.writers.iter().any(|w| w == role) {
            self.writers.push(role.to_owned());
        }
    }
    fn read(&mut self, role: &str) {
        if !self.readers.iter().any(|r| r == role) {
            self.readers.push(role.to_owned());
        }
    }
}

/// One proposal's worth of rollout state: project, attribution, the Judge task
/// that owns the claim, and the evidence spike the claim points at.
struct Deployment {
    project_id: String,
    user_id: String,
    proposal_id: String,
    judge_task_id: String,
    spike_task_id: String,
}

impl Deployment {
    fn claim(&self, question: &str) -> NeedsEvidenceClaim {
        NeedsEvidenceClaim {
            question: question.to_owned(),
            target_subsystem: "refinement".into(),
            spec_unknown_anchor: "typed evidence rollout".into(),
            insufficient_in_session_research: "spike required".into(),
            expected_findings: "canonical typed return".into(),
            round: 1,
            against_revision_seq: 1,
            created_by_task_id: self.judge_task_id.clone(),
        }
    }
}

async fn create_task(db: &Database, d: &(String, String), title: &str, issue_type: &str) -> String {
    TaskRepository::new(db.clone(), EventBus::noop())
        .create_in_project_with_provenance(
            &d.0,
            None,
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&d.1),
                source_task_id: None,
                proposal_id: None,
            },
            title,
            "Typed evidence rollout matrix fixture task",
            "",
            issue_type,
            0,
            "worker",
            Some("open"),
            Some("[]"),
        )
        .await
        .expect("create rollout fixture task")
        .id
}

/// Seed one deployment: project, user, proposal, Judge task, evidence spike.
async fn deployment(db: &Database) -> Deployment {
    let fixture = seed_refinement_fixture(db).await;
    let owner = (fixture.project_id.clone(), fixture.user_id.clone());
    let judge_task_id = create_task(db, &owner, "Judge for rollout", "refinement").await;
    let spike_task_id = create_task(db, &owner, "Evidence spike", "spike").await;
    Deployment {
        project_id: fixture.project_id,
        user_id: fixture.user_id,
        proposal_id: fixture.proposal_id,
        judge_task_id,
        spike_task_id,
    }
}

fn actor(
    db: &Database,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
) -> crate::actor::CoordinatorActor {
    build_refinement_actor(db, events_tx, spawn_test_pool(db, 4))
}

/// The coordinator's own evidence gate, derived from persisted authority.
async fn lifecycle_state(
    actor: &crate::actor::CoordinatorActor,
    proposal_id: &str,
) -> EvidenceLifecycleState {
    let proposal = actor
        .load_proposal_for_lifecycle(proposal_id)
        .await
        .expect("proposal must be readable for the lifecycle gate");
    actor.derive_proposal_evidence_lifecycle(&proposal).await
}

async fn advocate_task_count(db: &Database, project_id: &str) -> usize {
    TaskRepository::new(db.clone(), EventBus::noop())
        .list_by_project(project_id)
        .await
        .expect("list project tasks")
        .into_iter()
        .filter(|task| {
            task.issue_type == "refinement" && task.agent_type.as_deref() == Some("advocate")
        })
        .count()
}

async fn snapshot(db: &Database, d: &Deployment) -> TypedEvidenceRolloutSnapshotForTest {
    typed_evidence_rollout_snapshot_for_test(db, &d.proposal_id, &d.project_id).await
}

/// Drive the old (legacy) reader against a proposal that must still be
/// recoverable without any typed authority.
async fn old_reader_sees_link(db: &Database, d: &Deployment) {
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    assert_eq!(
        proposals
            .find_by_linked_spike(&d.spike_task_id)
            .await
            .expect("legacy reverse lookup")
            .map(|proposal| proposal.id)
            .as_deref(),
        Some(d.proposal_id.as_str()),
        "the old reader must resolve the proposal from its linked spike"
    );
    let candidate = proposals
        .list_linked_evidence_spike_recovery_candidates()
        .await
        .expect("legacy recovery inventory")
        .into_iter()
        .find(|candidate| candidate.proposal_id == d.proposal_id)
        .expect("the old reader must still see an active recovery candidate");
    assert_eq!(candidate.linked_spike_task_id, d.spike_task_id);
    assert_ne!(
        candidate.linked_spike_task_status, "closed",
        "the rollback reader must see the evidence task as still active"
    );
}

/// Prove the fail-closed read path: both repository projections refuse the
/// mismatched authority, the coordinator parks, neither dispatch path moves,
/// and nothing in the proposal's persisted state changed.
async fn assert_fails_closed_without_dispatch(
    db: &Database,
    d: &Deployment,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
) {
    let typed = TypedEvidenceRepository::new(db.clone());
    let before = snapshot(db, d).await;
    let advocates_before = advocate_task_count(db, &d.project_id).await;
    assert_eq!(
        typed
            .dual_read_legacy_parity(&d.proposal_id)
            .await
            .expect("dual read"),
        None,
        "mismatched authority must not project a parity result"
    );
    assert_eq!(
        typed
            .coordinator_lifecycle_projection(&d.proposal_id)
            .await
            .expect("lifecycle projection"),
        TypedEvidenceLifecycleProjection::Invalid,
        "mismatched authority must be invalid, never inferred"
    );
    let mut coordinator = actor(db, events_tx);
    seed_refinement_state(&mut coordinator, &d.proposal_id, Some(d.user_id.clone()));
    coordinator
        .active_refinements
        .get_mut(&d.proposal_id)
        .expect("refinement state")
        .phase = RefinementPhase::AdvocateRevision;
    assert_eq!(
        lifecycle_state(&coordinator, &d.proposal_id).await,
        EvidenceLifecycleState::AwaitingEvidence,
        "the coordinator must park on unreadable authority"
    );
    coordinator.redrive_demanded_evidence_dispatches().await;
    coordinator
        .dispatch_next_refinement_phase(&d.proposal_id)
        .await;
    assert_eq!(
        advocate_task_count(db, &d.project_id).await,
        advocates_before,
        "a fail-closed read must not dispatch a tribunal role"
    );
    assert_eq!(
        snapshot(db, d).await,
        before,
        "a fail-closed read must not mutate authority, attempts, or tasks"
    );
}

/// Assert that the new reader binds the exact finding, attempt, and task.
async fn assert_new_reader_binds_exact_allocation(
    db: &Database,
    d: &Deployment,
) -> (String, String) {
    let typed = TypedEvidenceRepository::new(db.clone());
    let parity = typed
        .dual_read_legacy_parity(&d.proposal_id)
        .await
        .expect("dual read")
        .expect("dual-written authority must project a parity result");
    let state = snapshot(db, d).await;
    assert_eq!(state.findings.len(), 1, "exactly one live typed finding");
    assert_eq!(state.attempts.len(), 1, "exactly one immutable attempt");
    assert_eq!(
        Some(parity.finding.id.as_str()),
        state.findings[0]["id"].as_str()
    );
    assert_eq!(
        parity.attempt_id.as_deref(),
        state.attempts[0]["id"].as_str(),
        "parity must bind the persisted attempt"
    );
    assert_eq!(
        parity.spike_task_id.as_deref(),
        Some(d.spike_task_id.as_str()),
        "parity must bind the exact evidence task"
    );
    assert_eq!(
        state.attempts[0]["spike_task_id"].as_str(),
        Some(d.spike_task_id.as_str())
    );
    assert_eq!(state.findings[0]["lifecycle"], "spike_active");
    match typed
        .coordinator_lifecycle_projection(&d.proposal_id)
        .await
        .expect("lifecycle projection")
    {
        TypedEvidenceLifecycleProjection::Valid(finding) => {
            assert_eq!(finding.id, parity.finding.id);
            assert_eq!(finding.lifecycle, TribunalEvidenceLifecycle::SpikeActive);
        }
        other => panic!("dual-written authority must project a live finding, got {other:?}"),
    }
    (parity.finding.id, parity.attempt_id.expect("attempt id"))
}

// ── Phase 1: legacy_only ────────────────────────────────────────────────────

async fn phase_legacy_only(
    db: &Database,
    d: &Deployment,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    log: &mut Executed,
) -> Value {
    let claim = serde_json::to_value(d.claim("Is the legacy-only shape recoverable?"))
        .expect("serialize legacy claim");
    write_legacy_only_evidence_authority_for_test(db, &d.proposal_id, &d.spike_task_id, &claim)
        .await;
    let written = snapshot(db, d).await;
    assert_eq!(
        written.legacy_link.as_deref(),
        Some(d.spike_task_id.as_str())
    );
    assert_eq!(written.legacy_claim.as_ref(), Some(&claim));
    assert!(
        written.findings.is_empty() && written.attempts.is_empty(),
        "the pre-typed writer produced no typed authority"
    );
    log.wrote("old");
    log.op("old_writer_sets_legacy_only_authority");

    old_reader_sees_link(db, d).await;
    log.read("old");
    log.op("old_reader_resolves_link_and_recovery_candidate");

    let typed = TypedEvidenceRepository::new(db.clone());
    assert_eq!(
        typed
            .dual_read_legacy_parity(&d.proposal_id)
            .await
            .expect("dual read"),
        None,
        "legacy-only authority has no typed counterpart to project"
    );
    // Absent typed authority under a live legacy link is not "no demand": the
    // coordinator's snapshot only accepts `Absent` when both legacy columns are
    // empty, so this row fails closed rather than resuming refinement.
    assert_eq!(
        typed
            .coordinator_lifecycle_projection(&d.proposal_id)
            .await
            .expect("lifecycle projection"),
        TypedEvidenceLifecycleProjection::Absent
    );
    let mut coordinator = actor(db, events_tx);
    seed_refinement_state(&mut coordinator, &d.proposal_id, Some(d.user_id.clone()));
    coordinator
        .active_refinements
        .get_mut(&d.proposal_id)
        .expect("refinement state")
        .phase = RefinementPhase::AdvocateRevision;
    assert_eq!(
        lifecycle_state(&coordinator, &d.proposal_id).await,
        EvidenceLifecycleState::AwaitingEvidence
    );
    log.read("new");
    log.op("new_reader_fails_closed_on_absent_typed_authority");

    assert!(
        typed
            .demanded_dispatches()
            .await
            .expect("dispatch inventory")
            .is_empty(),
        "a legacy-only row allocates no typed attempt to dispatch"
    );
    let tasks_before = task_row_count_for_test(db).await;
    coordinator.redrive_demanded_evidence_dispatches().await;
    assert_eq!(
        task_row_count_for_test(db).await,
        tasks_before,
        "restart re-drive must never invent a spike for legacy-only authority"
    );
    log.op("coordinator_redrive_dispatches_nothing");

    coordinator
        .dispatch_next_refinement_phase(&d.proposal_id)
        .await;
    assert_eq!(
        advocate_task_count(db, &d.project_id).await,
        0,
        "unreadable typed authority must block tribunal dispatch"
    );
    log.op("refinement_dispatch_stays_blocked");

    assert_eq!(
        snapshot(db, d).await,
        written,
        "legacy-only reads must leave the historical row untouched"
    );
    log.op("fail_closed_reads_mutated_nothing");
    claim
}

// ── Phase 2: dual_write_legacy_read ─────────────────────────────────────────

async fn phase_dual_write_legacy_read(
    db: &Database,
    d: &Deployment,
    claim: &Value,
    log: &mut Executed,
) {
    let typed = TypedEvidenceRepository::new(db.clone());
    let legacy_before = snapshot(db, d).await;
    let report = typed
        .backfill_active_legacy_evidence()
        .await
        .expect("backfill active legacy evidence");
    let after = snapshot(db, d).await;
    // Rows first: the report is the function describing itself, so it is
    // checked only after the persisted authority it claims to have written.
    assert_eq!(
        after.findings.len(),
        1,
        "backfill must persist typed authority for the active legacy row"
    );
    assert_eq!(after.findings[0]["lifecycle"], "spike_active");
    assert_eq!(after.findings[0]["claim"], *claim);
    assert_eq!(
        after.findings[0]["demand_hash"].as_str(),
        Some(legacy_demand_hash(claim, Some(&d.spike_task_id)).as_str()),
        "the backfilled finding must carry the legacy demand identity"
    );
    assert_eq!(
        after.findings[0]["created_by_task_id"].as_str(),
        Some(d.judge_task_id.as_str())
    );
    assert_eq!(after.attempts.len(), 1);
    assert_eq!(after.attempts[0]["sequence"], 1);
    assert_eq!(
        after.attempts[0]["spike_task_id"].as_str(),
        Some(d.spike_task_id.as_str())
    );
    let transitions: Vec<(&str, Option<&str>, &str)> = after
        .transitions
        .iter()
        .map(|t| {
            (
                t["to_lifecycle"].as_str().unwrap_or_default(),
                t["from_lifecycle"].as_str(),
                t["metadata"]["source"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        transitions,
        vec![
            ("demanded", None, "legacy_backfill"),
            ("spike_active", Some("demanded"), "legacy_backfill"),
        ],
        "backfill must record the demand and allocation it actually performed"
    );
    assert!(
        report.created_findings >= 1 && report.created_attempts >= 1,
        "the backfill report must account for the rows it wrote, got {report:?}"
    );
    log.wrote("new");
    log.op("backfill_dual_writes_typed_authority");

    assert_eq!(after.legacy_link, legacy_before.legacy_link);
    assert_eq!(after.legacy_claim, legacy_before.legacy_claim);
    log.op("legacy_authority_survives_dual_write");

    old_reader_sees_link(db, d).await;
    log.read("old");
    log.op("old_reader_still_resolves_link_and_recovery_candidate");

    // The old writer is still deployed during dual write: it rewrites the same
    // compatibility authority, and the new reader must still bind the exact
    // typed allocation afterwards.
    overwrite_legacy_evidence_authority_for_test(
        db,
        &d.proposal_id,
        Some(&d.spike_task_id),
        Some(claim),
    )
    .await;
    log.wrote("old");
    assert_new_reader_binds_exact_allocation(db, d).await;
    assert_eq!(
        snapshot(db, d).await,
        after,
        "an old-writer rewrite during dual write must change nothing"
    );
    log.read("new");
    log.op("old_writer_rewrite_during_dual_write_keeps_parity");

    let rerun = typed
        .backfill_active_legacy_evidence()
        .await
        .expect("rerun backfill");
    assert_eq!(
        (rerun.created_findings, rerun.created_attempts),
        (0, 0),
        "a re-run backfill must create nothing, got {rerun:?}"
    );
    assert_eq!(
        snapshot(db, d).await,
        after,
        "backfill must be byte-identical on re-run"
    );
    log.op("backfill_rerun_is_idempotent");
}

// ── Phase 3: dual_write_dual_read ───────────────────────────────────────────

async fn phase_dual_write_dual_read(
    db: &Database,
    d: &Deployment,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    log: &mut Executed,
) {
    assert_new_reader_binds_exact_allocation(db, d).await;
    log.read("new");
    log.op("dual_read_binds_exact_finding_attempt_and_task");

    old_reader_sees_link(db, d).await;
    log.read("old");
    log.op("old_reader_agrees_with_new_reader");

    let parity_state = snapshot(db, d).await;
    // Mixed-version drift: a rollback-era writer repoints the compatibility
    // link at a different active spike while the typed attempt still owns the
    // original one.
    let other_spike = create_task(
        db,
        &(d.project_id.clone(), d.user_id.clone()),
        "Competing evidence spike",
        "spike",
    )
    .await;
    overwrite_legacy_evidence_authority_for_test(
        db,
        &d.proposal_id,
        Some(&other_spike),
        parity_state.legacy_claim.as_ref(),
    )
    .await;
    log.wrote("old");
    assert_fails_closed_without_dispatch(db, d, events_tx).await;
    log.op("induced_task_mismatch_fails_closed_without_dispatch");

    overwrite_legacy_evidence_authority_for_test(
        db,
        &d.proposal_id,
        parity_state.legacy_link.as_deref(),
        parity_state.legacy_claim.as_ref(),
    )
    .await;
    assert_new_reader_binds_exact_allocation(db, d).await;
    log.op("restored_link_restores_parity");

    let mut restored = snapshot(db, d).await;
    // The competing spike is a fixture artifact of the injected drift, not a
    // replacement task the system created.
    restored
        .tasks
        .retain(|task| task["id"].as_str() != Some(other_spike.as_str()));
    assert_eq!(
        restored, parity_state,
        "the drift injection and fail-closed cycle changed no typed authority"
    );
    log.op("fail_closed_reads_mutated_nothing");
}

// ── Phase 4: typed_write_dual_read ──────────────────────────────────────────

async fn phase_typed_write_dual_read(
    db: &Database,
    d: &Deployment,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    log: &mut Executed,
) {
    let claim = d.claim("Does the typed writer stay legacy-compatible?");
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_structured_needs_evidence_spike(&d.proposal_id, &d.spike_task_id, &claim)
        .await
        .expect("typed writer dual-writes evidence authority");
    let claim_value = serde_json::to_value(&claim).expect("serialize claim");
    let written = snapshot(db, d).await;
    assert_eq!(
        written.legacy_link.as_deref(),
        Some(d.spike_task_id.as_str())
    );
    assert_eq!(written.legacy_claim.as_ref(), Some(&claim_value));
    assert_eq!(written.findings.len(), 1);
    assert_eq!(written.findings[0]["lifecycle"], "spike_active");
    assert_eq!(written.attempts.len(), 1);
    assert_eq!(
        written.attempts[0]["spike_task_id"].as_str(),
        Some(d.spike_task_id.as_str())
    );
    log.wrote("new");
    log.op("new_writer_dual_writes_typed_and_legacy");

    old_reader_sees_link(db, d).await;
    log.read("old");
    log.op("old_reader_resolves_new_writer_authority");

    assert_new_reader_binds_exact_allocation(db, d).await;
    log.read("new");
    log.op("new_reader_binds_exact_finding_attempt_and_task");

    // Mixed-version drift on the other axis: the claim itself is rewritten
    // under the typed finding.
    let drifted = serde_json::to_value(d.claim("A different load-bearing question"))
        .expect("serialize drifted claim");
    overwrite_legacy_evidence_authority_for_test(
        db,
        &d.proposal_id,
        Some(&d.spike_task_id),
        Some(&drifted),
    )
    .await;
    log.wrote("old");
    assert_fails_closed_without_dispatch(db, d, events_tx).await;
    log.op("induced_claim_mismatch_fails_closed_without_dispatch");

    overwrite_legacy_evidence_authority_for_test(
        db,
        &d.proposal_id,
        Some(&d.spike_task_id),
        Some(&claim_value),
    )
    .await;
    assert_new_reader_binds_exact_allocation(db, d).await;
    log.op("restored_claim_restores_parity");

    assert_eq!(
        snapshot(db, d).await,
        written,
        "the claim drift cycle changed no typed authority"
    );
    log.op("fail_closed_reads_mutated_nothing");
}

// ── Phase 5: typed_only ─────────────────────────────────────────────────────

async fn phase_typed_only(
    db: &Database,
    d: &Deployment,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    log: &mut Executed,
) {
    let tasks = TaskRepository::new(db.clone(), EventBus::noop());
    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    let typed = TypedEvidenceRepository::new(db.clone());
    let dual = snapshot(db, d).await;
    tasks
        .set_status_with_reason(&d.spike_task_id, "closed", Some("completed"))
        .await
        .expect("terminal evidence task");
    typed
        .evidence_received_and_clear_legacy(&d.proposal_id, &d.spike_task_id)
        .await
        .expect("typed receipt clears legacy authority");
    let cleared = snapshot(db, d).await;
    assert_eq!(cleared.legacy_link, None, "legacy link retired");
    assert_eq!(cleared.legacy_claim, None, "legacy claim retired");
    assert_eq!(cleared.findings[0]["lifecycle"], "evidence_received");
    assert_eq!(
        cleared.attempts, dual.attempts,
        "retiring legacy authority must not rewrite immutable attempts"
    );
    assert_eq!(
        cleared.transitions.len(),
        dual.transitions.len() + 1,
        "exactly one receipt transition is appended"
    );
    assert_eq!(
        cleared.transitions[..dual.transitions.len()],
        dual.transitions[..],
        "typed history is append-only across the legacy retirement"
    );
    let receipt = cleared.transitions.last().expect("receipt transition");
    assert_eq!(receipt["to_lifecycle"], "evidence_received");
    assert_eq!(receipt["metadata"]["source"], "legacy_dual_write_clear");
    log.wrote("new");
    log.op("typed_writer_clears_legacy_on_receipt");

    assert!(
        proposals
            .find_by_linked_spike(&d.spike_task_id)
            .await
            .expect("legacy reverse lookup")
            .is_none(),
        "the old reader must find no legacy authority once typed is sole authority"
    );
    assert!(
        !proposals
            .list_linked_evidence_spike_recovery_candidates()
            .await
            .expect("legacy recovery inventory")
            .iter()
            .any(|candidate| candidate.proposal_id == d.proposal_id),
        "a typed-only proposal is not a legacy recovery candidate"
    );
    assert!(
        proposals
            .current_evidence_findings_for_linked_spike(&d.proposal_id, &d.spike_task_id)
            .await
            .expect("legacy findings read")
            .is_none(),
        "the old reader cannot resume a typed-only proposal"
    );
    log.read("old");
    log.op("old_reader_has_no_legacy_authority_left");

    match typed
        .coordinator_lifecycle_projection(&d.proposal_id)
        .await
        .expect("lifecycle projection")
    {
        TypedEvidenceLifecycleProjection::Valid(finding) => {
            assert_eq!(
                finding.lifecycle,
                TribunalEvidenceLifecycle::EvidenceReceived
            );
            assert_eq!(
                Some(finding.id.as_str()),
                cleared.findings[0]["id"].as_str()
            );
        }
        other => panic!("typed-only authority must project a live finding, got {other:?}"),
    }
    let mut coordinator = actor(db, events_tx);
    seed_refinement_state(&mut coordinator, &d.proposal_id, Some(d.user_id.clone()));
    coordinator
        .active_refinements
        .get_mut(&d.proposal_id)
        .expect("refinement state")
        .phase = RefinementPhase::AdvocateRevision;
    assert_eq!(
        lifecycle_state(&coordinator, &d.proposal_id).await,
        EvidenceLifecycleState::EvidenceReady
    );
    log.read("new");
    log.op("new_reader_is_sole_authority");

    // Negative control: a rollback-era writer re-installing legacy authority
    // over a typed-only finding must make the read invalid and stop dispatch.
    let stale = serde_json::to_value(d.claim("A resurrected legacy claim")).expect("serialize");
    overwrite_legacy_evidence_authority_for_test(
        db,
        &d.proposal_id,
        Some(&d.spike_task_id),
        Some(&stale),
    )
    .await;
    let before_blocked = snapshot(db, d).await;
    assert_eq!(
        typed
            .coordinator_lifecycle_projection(&d.proposal_id)
            .await
            .expect("lifecycle projection"),
        TypedEvidenceLifecycleProjection::Invalid
    );
    assert_eq!(
        lifecycle_state(&coordinator, &d.proposal_id).await,
        EvidenceLifecycleState::AwaitingEvidence
    );
    coordinator
        .dispatch_next_refinement_phase(&d.proposal_id)
        .await;
    assert_eq!(
        advocate_task_count(db, &d.project_id).await,
        0,
        "resurrected legacy authority must block the Advocate fold"
    );
    assert_eq!(
        snapshot(db, d).await,
        before_blocked,
        "the blocked dispatch mutated nothing"
    );
    log.wrote("old");
    log.op("reintroduced_legacy_authority_fails_closed_and_blocks_dispatch");

    // Positive control: with typed-only authority restored the same call
    // dispatches, so the block above is the read failing closed rather than an
    // unrelated gate.
    overwrite_legacy_evidence_authority_for_test(db, &d.proposal_id, None, None).await;
    assert_eq!(
        lifecycle_state(&coordinator, &d.proposal_id).await,
        EvidenceLifecycleState::EvidenceReady
    );
    coordinator
        .dispatch_next_refinement_phase(&d.proposal_id)
        .await;
    assert_eq!(
        advocate_task_count(db, &d.project_id).await,
        1,
        "typed-only receipt must admit exactly the Advocate fold"
    );
    log.op("typed_only_authority_admits_advocate_dispatch");
}

// ── Phase 6: reverse_rollback ───────────────────────────────────────────────

async fn phase_reverse_rollback(
    db: &Database,
    d: &Deployment,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    log: &mut Executed,
) {
    let typed = TypedEvidenceRepository::new(db.clone());
    let claim = d.claim("Does an active spike survive a reverse rollback?");
    ProposalRepository::new(db.clone(), EventBus::noop())
        .set_structured_needs_evidence_spike(&d.proposal_id, &d.spike_task_id, &claim)
        .await
        .expect("dual-write the active evidence demand");
    let written = snapshot(db, d).await;
    assert_eq!(written.findings.len(), 1);
    assert_eq!(written.findings[0]["lifecycle"], "spike_active");
    assert_eq!(
        written.attempts[0]["spike_task_id"].as_str(),
        Some(d.spike_task_id.as_str())
    );
    log.wrote("new");
    log.op("new_writer_dual_writes_active_demand");
    // The spike captured its immutable plan and command invocation before the
    // rollback; those facts are what the new reader must still be able to
    // hydrate afterwards.
    let delivery = seed_canonical_typed_evidence_ingress_fixture_for_test(
        db,
        &d.proposal_id,
        &d.spike_task_id,
        "rollback-check",
        djinn_db::test_support::CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
    )
    .await;

    // ── The rollback moment: the new writer is gone; only persisted rows
    // remain, and the evidence task is still open.
    let rolled_back = snapshot(db, d).await;
    assert_eq!(
        rolled_back.legacy_link.as_deref(),
        Some(d.spike_task_id.as_str()),
        "the active legacy link must not be cleared by a rollback"
    );
    assert_eq!(
        rolled_back.legacy_claim.as_ref(),
        Some(&serde_json::to_value(&claim).expect("serialize claim")),
        "the active legacy claim must not be cleared by a rollback"
    );
    assert_eq!(
        rolled_back
            .tasks
            .iter()
            .find(|task| task["id"].as_str() == Some(d.spike_task_id.as_str()))
            .expect("evidence task row")["status"],
        "open",
        "the rollback is executed while the evidence task is active"
    );
    old_reader_sees_link(db, d).await;
    log.read("old");
    log.op("rollback_keeps_old_reader_legacy_availability");

    let (finding_id, attempt_id) = assert_new_reader_binds_exact_allocation(db, d).await;
    assert_eq!(finding_id, delivery.finding_id);
    assert_eq!(attempt_id, delivery.attempt_id);
    assert!(
        typed
            .has_active_attempt_for_task(&d.spike_task_id)
            .await
            .expect("delivery fence"),
        "the exact evidence task must remain the active attempt's producer"
    );
    log.read("new");
    log.op("rollback_keeps_exact_active_attempt_binding");

    let invocations = evidence_plan_invocation_availability_for_test(db, &attempt_id).await;
    assert_eq!(
        invocations.len(),
        1,
        "the attempt's immutable planned check must still resolve its plan"
    );
    let invocation = &invocations[0];
    assert_eq!(
        invocation["plan_spike_task_id"].as_str(),
        Some(d.spike_task_id.as_str())
    );
    assert_eq!(invocation["method"], "command");
    let invocation_id = invocation["invocation_id"]
        .as_str()
        .expect("the immutable command invocation must survive the rollback")
        .to_owned();
    let plan_id = invocation["evidence_plan_id"]
        .as_str()
        .expect("plan identity")
        .to_owned();
    assert_eq!(invocation["launch_state"], "launched");
    assert_eq!(invocation["process_state"], "exited");
    assert_eq!(invocation["exit_code"], 0);
    assert_eq!(invocation["timed_out"], false);
    log.op("rollback_keeps_immutable_evidence_plan_invocation");

    // Roll forward: restart re-drive must not strand, replace, or duplicate
    // the exact allocation that was live across the rollback.
    let mut coordinator = actor(db, events_tx);
    let tasks_before = task_row_count_for_test(db).await;
    coordinator.redrive_demanded_evidence_dispatches().await;
    assert!(
        coordinator
            .recover_terminal_linked_spike_evidence()
            .await
            .is_empty(),
        "an open evidence task has produced no terminal delivery to replay"
    );
    assert_eq!(
        task_row_count_for_test(db).await,
        tasks_before,
        "restart re-drive must not create a replacement spike"
    );
    assert_eq!(
        snapshot(db, d).await,
        rolled_back,
        "restart re-drive must not strand or rewrite the live allocation"
    );
    assert_eq!(
        evidence_plan_invocation_availability_for_test(db, &attempt_id).await,
        invocations,
        "restart re-drive must not disturb immutable evidence facts"
    );
    log.op("restart_redrive_strands_nothing_and_replaces_nothing");

    // Availability proven by use: the rollback is over, the exact same spike
    // finishes normally, and the production validator hydrates the immutable
    // pre-rollback invocation as its anchor.
    TaskRepository::new(db.clone(), EventBus::noop())
        .set_status_with_reason(&d.spike_task_id, "closed", Some("completed"))
        .await
        .expect("the exact post-rollback spike settles");
    let result = typed
        .submit_return_v1_for_task(&d.spike_task_id, delivery.return_payload.as_bytes())
        .await
        .expect("the exact post-rollback attempt must still deliver");
    assert!(!result.replayed);
    let validation = typed_evidence_validation_snapshot_for_test(db, &result.validation_id).await;
    assert_eq!(validation.finding_lifecycle, "evidence_received");
    assert_eq!(validation.check_anchors.len(), 1);
    assert_eq!(
        validation.check_anchors[0]["immutable_identity"]["invocation_id"].as_str(),
        Some(invocation_id.as_str()),
        "the anchor must hydrate the exact pre-rollback invocation"
    );
    assert_eq!(
        validation.check_anchors[0]["immutable_identity"]["evidence_plan_id"].as_str(),
        Some(plan_id.as_str())
    );
    assert_eq!(validation.check_anchors[0]["method_compatible"], true);
    assert_eq!(validation.checks[0]["invocation_usable"], true);
    let delivered = snapshot(db, d).await;
    assert_eq!(
        delivered.attempts, rolled_back.attempts,
        "delivery must bind the exact pre-rollback attempt, not a replacement"
    );
    log.op("new_reader_hydrates_immutable_anchors_after_rollback");

    // The other rollback shape: an allocation that committed but had not yet
    // dispatched when the rollback happened.
    let allocation = materialize_evidence_dispatch_recovery_fixture_for_test(db, false).await;
    let before: EvidenceDispatchRecoverySnapshotForTest =
        evidence_dispatch_recovery_snapshot_for_test(db, &allocation).await;
    assert_eq!(before.lifecycle, "demanded");
    assert_eq!(
        before.legacy_link.as_deref(),
        Some(allocation.spike_task_id.as_str())
    );
    assert_eq!(
        typed
            .demanded_dispatches()
            .await
            .expect("dispatch inventory")
            .into_iter()
            .map(|dispatch| (
                dispatch.finding_id,
                dispatch.attempt_id,
                dispatch.spike_task_id
            ))
            .collect::<Vec<_>>(),
        vec![(
            allocation.finding_id.clone(),
            allocation.attempt_id.clone(),
            allocation.spike_task_id.clone()
        )],
        "the exact stranded allocation is the only thing to re-drive"
    );
    set_evidence_dispatch_test_script(
        &allocation.spike_task_id,
        [
            EvidenceDispatchTestOutcome::EnqueueFailed,
            EvidenceDispatchTestOutcome::Accepted,
        ],
        false,
    );
    let tasks_before = task_row_count_for_test(db).await;
    coordinator.redrive_demanded_evidence_dispatches().await;
    let failed = evidence_dispatch_recovery_snapshot_for_test(db, &allocation).await;
    assert_eq!(
        failed.lifecycle, "demanded",
        "a failed enqueue never activates"
    );
    assert_eq!(failed.dispatch_error_count, 1);
    assert_eq!(failed.attempt_id, before.attempt_id);
    assert_eq!(failed.spike_task_id, before.spike_task_id);
    assert_eq!(failed.legacy_link, before.legacy_link);
    coordinator.redrive_demanded_evidence_dispatches().await;
    let dispatched = evidence_dispatch_recovery_snapshot_for_test(db, &allocation).await;
    assert_eq!(dispatched.lifecycle, "spike_active");
    assert_eq!(dispatched.attempt_id, before.attempt_id);
    assert_eq!(dispatched.attempt_sequence, before.attempt_sequence);
    assert_eq!(dispatched.spike_task_id, before.spike_task_id);
    assert_eq!(dispatched.attempt_count, 1, "no replacement attempt");
    assert_eq!(dispatched.finding_slot_count, 1, "no replacement finding");
    assert_eq!(dispatched.dispatch_error_count, 1);
    assert_eq!(
        dispatched.legacy_link, before.legacy_link,
        "dispatch must not prematurely clear the compatibility link"
    );
    assert_ne!(dispatched.task_status, "closed");
    assert_eq!(
        task_row_count_for_test(db).await,
        tasks_before,
        "re-drive must dispatch the exact task, never create a replacement"
    );
    assert_eq!(evidence_dispatch_test_count(&allocation.spike_task_id), 2);
    log.op("demanded_allocation_redrives_exact_task_without_replacement");
}

// ── The matrix ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_rollout_matrix() {
    let fixture: RolloutFixture =
        serde_json::from_str(ROLLOUT_MATRIX).expect("valid rollout matrix fixture");
    assert_eq!(fixture.fixture, "typed_evidence_rollout_matrix");
    assert_eq!(
        fixture
            .phases
            .iter()
            .map(|row| row.phase.as_str())
            .collect::<Vec<_>>(),
        REQUIRED_PHASES,
        "the rollout fixture must declare all six deployment phases in rollout order"
    );

    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    // The legacy-origin rollout (phases 1-3) migrates one proposal forward; the
    // typed-origin deployment (phases 4-5) and the rollback deployment (phase 6)
    // are separate proposals because their authority starts in different eras.
    let legacy_origin = deployment(&db).await;
    let typed_origin = deployment(&db).await;
    let rollback = deployment(&db).await;
    let mut legacy_claim = Value::Null;
    let mut executed: Vec<String> = Vec::new();

    for row in &fixture.phases {
        let mut log = Executed::default();
        match row.phase.as_str() {
            "legacy_only" => {
                legacy_claim = phase_legacy_only(&db, &legacy_origin, &events_tx, &mut log).await;
            }
            "dual_write_legacy_read" => {
                phase_dual_write_legacy_read(&db, &legacy_origin, &legacy_claim, &mut log).await;
            }
            "dual_write_dual_read" => {
                phase_dual_write_dual_read(&db, &legacy_origin, &events_tx, &mut log).await;
            }
            "typed_write_dual_read" => {
                phase_typed_write_dual_read(&db, &typed_origin, &events_tx, &mut log).await;
            }
            "typed_only" => {
                phase_typed_only(&db, &typed_origin, &events_tx, &mut log).await;
            }
            "reverse_rollback" => {
                phase_reverse_rollback(&db, &rollback, &events_tx, &mut log).await;
            }
            other => panic!("rollout phase {other} has no executor; it cannot be attested"),
        }
        assert_eq!(
            log.ops, row.program,
            "phase {} executed a different program than it declares",
            row.phase
        );
        let mut writers = log.writers.clone();
        writers.sort();
        let mut declared_writers = row.writers.clone();
        declared_writers.sort();
        assert_eq!(
            writers, declared_writers,
            "phase {} exercised different writer eras than it declares",
            row.phase
        );
        let mut readers = log.readers.clone();
        readers.sort();
        let mut declared_readers = row.readers.clone();
        declared_readers.sort();
        assert_eq!(
            readers, declared_readers,
            "phase {} exercised different reader eras than it declares",
            row.phase
        );
        executed.push(row.phase.clone());
    }

    assert_eq!(
        executed, REQUIRED_PHASES,
        "every deployment phase must have been executed, not merely declared"
    );
}
