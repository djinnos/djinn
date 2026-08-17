//! Executable typed-evidence gate matrix.
//!
//! Every row of `fixtures/typed_evidence_gate_matrix.json` is driven through
//! all five gated transitions — `proposal_debate_append` (Judge verdict),
//! `proposal_refinement_resolve`, `proposal_signoff`,
//! `proposal_verdict_override`, `proposal_graduate` — plus the composed gate
//! itself, against a migrated database.
//!
//! The fixture's `expected` field is an assertion, never a source of truth. A
//! row is judged on the **persisted** row each transition would have written:
//! the sign-off row, the debate verdict row, the override lifecycle row, and
//! the proposal status plus its `epic_breakdown` task. A row therefore cannot
//! pass by the test reading declarative JSON back to itself.
//!
//! Every lifecycle state is reached through a production writer —
//! `set_structured_needs_evidence_spike`, `submit_return_v1_for_task`,
//! `dispose_in_transaction` — never by writing a lifecycle string.

use djinn_control_plane::server::DjinnMcpServer;
use djinn_control_plane::state::stubs::test_mcp_state;
use djinn_control_plane::tools::proposal_tools::TypedEvidenceGateMode;
use djinn_control_plane::tools::proposal_tools::signoff::evaluate_composed_gate;
use djinn_core::events::EventBus;
use djinn_core::models::{NeedsEvidenceClaim, TribunalEvidenceLifecycle};
use djinn_db::{
    AdmitRefinementRunRequest, AtomicEvidenceDispositionInput, Database, DemandTypedEvidenceInput,
    ProjectRepository, ProposalCreateInput, ProposalDebateTrailCreateInput, ProposalRepository,
    ProposalUpdateInput, RefinementAdmissionOutcome, RefinementAdmissionSource, TaskRepository,
    TypedEvidenceRepository, legacy_demand_hash,
    test_support::{
        CanonicalTypedEvidenceReturnOutcomeForTest, UsageTestTaskSeed,
        dispose_typed_evidence_validation_for_test, materialize_judge_authority_for_test,
        seed_canonical_typed_evidence_ingress_fixture_for_test, seed_task_row,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

const MATRIX: &str = include_str!("fixtures/typed_evidence_gate_matrix.json");

/// A body that clears every deterministic DoR check, so the composed gate
/// reaches its tribunal section instead of short-circuiting on readiness.
const READY_BODY: &str = r#"
# Problem
A settled-looking proposal can still be resting on an unproven claim.

# Scope
In scope: the five gated transitions. Out of scope: coordinator dispatch.

# Objectives
- Refuse every settlement while typed evidence is unresolved
- Admit every settlement once the Judge has disposed of it

## File map
```file-map
    server/crates/djinn-control-plane/src/tools/proposal_tools/signoff.rs
    server/crates/djinn-db/src/repositories/typed_evidence.rs
```

# Dependencies
Blocked by the typed evidence projection in `djinn-db`.

# Open Questions
Does a prose verdict settle a claim?
"#;

const CLAIM_QUESTION: &str = "Can the launcher share a cgroup across pods?";

#[derive(Debug, Deserialize)]
struct Matrix {
    version: String,
    transitions: Vec<String>,
    rows: Vec<Row>,
}

#[derive(Clone, Debug, Deserialize)]
struct Row {
    name: String,
    lifecycle: String,
    evidence_outcome: Option<String>,
    revision_offset: i32,
    verdict_form: String,
    expected: String,
}

impl Row {
    fn expects_block(&self) -> bool {
        match self.expected.as_str() {
            "blocked" => true,
            "admitted" => false,
            other => panic!("row {} has unknown expectation {other:?}", self.name),
        }
    }
}

fn matrix() -> Matrix {
    let matrix: Matrix = serde_json::from_str(MATRIX).expect("matrix fixture is valid");
    assert_eq!(matrix.version, "typed_evidence_gate_matrix_v1");
    matrix
}

/// One proposal carrying exactly one row's lifecycle state.
struct Case {
    db: Database,
    proposals: ProposalRepository,
    project_id: String,
    proposal_id: String,
    user_id: String,
    /// Id of the finding, whatever lifecycle it ended in.
    finding_id: String,
    /// `None` for a row whose finding reached a terminal lifecycle.
    blocking_finding_id: Option<String>,
}

/// A database shared by every case in one test function. Creating one costs a
/// full migration pass, so the matrix seeds many proposals into one instead.
struct Harness {
    db: Database,
    project_id: String,
    user_id: String,
}

impl Harness {
    async fn new() -> Self {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let users = djinn_db::UserRepository::new(db.clone());
        let user = users
            .upsert_from_github(
                900_000 + i64::from(std::process::id() % 90_000),
                &format!("matrix-{}", uuid::Uuid::now_v7()),
                None,
                None,
            )
            .await
            .unwrap();
        users.set_role(&user.id, "engineer").await.unwrap();
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create(
                &format!("svc-{}", uuid::Uuid::now_v7()),
                "test",
                &format!("svc-{}", uuid::Uuid::now_v7()),
            )
            .await
            .unwrap();
        Self {
            db,
            project_id: project.id,
            user_id: user.id,
        }
    }

    /// Build one proposal in this row's lifecycle state.
    async fn case(&self, row: &Row) -> Case {
        let proposals = ProposalRepository::new(self.db.clone(), EventBus::noop());
        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(self.user_id.clone()), async {
                proposals
                    .create(ProposalCreateInput {
                        title: &format!("Matrix {}", row.name),
                        body: READY_BODY,
                        acceptance_criteria: Some(
                            r#"[{"criterion":"The gate names the blocking finding","met":false}]"#,
                        ),
                        status: None,
                        body_format: None,
                    })
                    .await
            })
            .await
            .unwrap();
        proposals
            .add_target(&proposal.id, &self.project_id, "primary")
            .await
            .unwrap();
        let spike_task_id = seed_task_row(
            &self.db,
            UsageTestTaskSeed {
                project_id: &self.project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        let mut case = Case {
            db: self.db.clone(),
            proposals,
            project_id: self.project_id.clone(),
            proposal_id: proposal.id,
            user_id: self.user_id.clone(),
            finding_id: String::new(),
            blocking_finding_id: None,
        };
        case.drive_to(row, &spike_task_id).await;
        case
    }
}

impl Case {
    fn claim(&self, spike_task_id: &str) -> NeedsEvidenceClaim {
        NeedsEvidenceClaim {
            question: CLAIM_QUESTION.into(),
            target_subsystem: "launcher".into(),
            spec_unknown_anchor: "cgroup delegation".into(),
            insufficient_in_session_research: "needs a live kernel probe".into(),
            expected_findings: "a delegated cgroup or a kernel refusal".into(),
            created_by_task_id: spike_task_id.to_owned(),
            round: 1,
            against_revision_seq: 1,
        }
    }

    /// Reach the row's lifecycle using production writers only.
    async fn drive_to(&mut self, row: &Row, spike_task_id: &str) {
        let typed = TypedEvidenceRepository::new(self.db.clone());
        if row.lifecycle == "demanded" {
            // `demanded` is the pre-allocation state: a typed demand with no
            // spike yet, dual-written to the legacy compatibility claim.
            let claim = serde_json::to_value(self.claim(spike_task_id)).unwrap();
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
                    judge_task_id: spike_task_id.to_owned(),
                },
                None,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            self.finding_id = finding_id;
        } else {
            self.proposals
                .set_structured_needs_evidence_spike(
                    &self.proposal_id,
                    spike_task_id,
                    &self.claim(spike_task_id),
                )
                .await
                .unwrap();
            self.finding_id = typed
                .unresolved_projection(&self.proposal_id)
                .await
                .unwrap()
                .expect("the structured demand must project")
                .finding_id;
        }

        let needs_return = matches!(
            row.lifecycle.as_str(),
            "evidence_received" | "failed" | "resolved" | "withdrawn"
        );
        let mut validation_id = None;
        if needs_return {
            let outcome = match row.evidence_outcome.as_deref() {
                Some("partial") => CanonicalTypedEvidenceReturnOutcomeForTest::Partial,
                Some("unresolved") => CanonicalTypedEvidenceReturnOutcomeForTest::Unresolved,
                _ => CanonicalTypedEvidenceReturnOutcomeForTest::Resolved,
            };
            let ingress = seed_canonical_typed_evidence_ingress_fixture_for_test(
                &self.db,
                &self.proposal_id,
                spike_task_id,
                "matrix",
                outcome,
            )
            .await;
            // The durable return path authenticates a closed spike task.
            TaskRepository::new(self.db.clone(), EventBus::noop())
                .set_status_with_reason(spike_task_id, "closed", Some("completed"))
                .await
                .unwrap();
            if row.lifecycle == "failed" {
                // Reach `failed` through the real ingress rejection branch: a
                // payload the validator refuses records an append-only failure.
                let mut payload: Value = serde_json::from_str(&ingress.return_payload).unwrap();
                payload["conclusion"] = json!("");
                typed
                    .submit_return_v1_for_task(
                        spike_task_id,
                        serde_json::to_string(&payload).unwrap().as_bytes(),
                    )
                    .await
                    .expect_err("a malformed return must be rejected");
            } else {
                validation_id = Some(
                    typed
                        .submit_return_v1_for_task(spike_task_id, ingress.return_payload.as_bytes())
                        .await
                        .unwrap()
                        .validation_id,
                );
            }
        }

        // Advance the spec head past the demand before disposing, so a terminal
        // row also proves the disposition is not revision-scoped.
        self.advance_revisions(row.revision_offset).await;

        if matches!(row.lifecycle.as_str(), "resolved" | "withdrawn") {
            let disposition = if row.lifecycle == "resolved" {
                TribunalEvidenceLifecycle::Resolved
            } else {
                TribunalEvidenceLifecycle::Withdrawn
            };
            let judge_task_id = self.materialize_judge().await;
            dispose_typed_evidence_validation_for_test(
                &self.db,
                validation_id
                    .as_deref()
                    .expect("terminal rows carry a validation"),
                &judge_task_id,
                disposition,
            )
            .await;
        }

        if row.verdict_form == "prose_only" {
            // Prose, not a disposition. Recorded through the repository because
            // the MCP tool that would write it is itself gated.
            self.proposals
                .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                    proposal_id: &self.proposal_id,
                    kind: "verdict",
                    body: "approve: the cgroup claim is settled; no further evidence is required",
                    blocking: false,
                    agent_role: "judge",
                    author_kind: "agent",
                    author_model: Some("matrix-judge"),
                    source_task_id: None,
                    against_revision_seq: 1,
                    round: 1,
                    body_metadata: None,
                })
                .await
                .unwrap();
        }

        self.blocking_finding_id = TypedEvidenceRepository::new(self.db.clone())
            .unresolved_projection(&self.proposal_id)
            .await
            .unwrap()
            .map(|projection| projection.finding_id);
        // The fixture's expectation must match the repository's own view of
        // whether anything is still unresolved, or the matrix is asserting
        // against a state it failed to build.
        assert_eq!(
            self.blocking_finding_id.is_some(),
            row.expects_block(),
            "row {} did not reach the lifecycle it declares",
            row.name
        );
    }

    /// Commit `count` further spec revisions after the demand was raised.
    async fn advance_revisions(&self, count: i32) {
        for step in 1..=count {
            let proposal = self
                .proposals
                .get(&self.proposal_id)
                .await
                .unwrap()
                .unwrap();
            self.proposals
                .update(
                    &self.proposal_id,
                    ProposalUpdateInput {
                        title: &proposal.title,
                        body: &format!("{READY_BODY}\n<!-- revision step {step} -->"),
                        acceptance_criteria: &proposal.acceptance_criteria,
                        status: &proposal.status,
                        superseded_by: None,
                        body_format: Some(&proposal.body_format),
                        event_metadata: None,
                    },
                )
                .await
                .unwrap();
        }
        let head = self
            .proposals
            .get(&self.proposal_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            head.latest_revision_seq,
            1 + count,
            "the fixture must actually advance the head"
        );
    }

    /// An active Judge on a running refinement run — the only attribution the
    /// repository accepts for a terminal disposition.
    async fn materialize_judge(&self) -> String {
        self.proposals
            .record_refinement_lifecycle(&self.proposal_id, "refinement_start", None)
            .await
            .unwrap();
        let (run_id, generation) = match self
            .proposals
            .reap_and_admit(AdmitRefinementRunRequest {
                proposal_id: self.proposal_id.clone(),
                idempotency_key: format!("matrix/{}", self.proposal_id),
                source: RefinementAdmissionSource::Demand {
                    demand_id: format!("matrix/{}", self.proposal_id),
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
        judge_task_id
    }

    fn server(&self) -> DjinnMcpServer {
        DjinnMcpServer::new(
            test_mcp_state(self.db.clone())
                .with_typed_evidence_gate_mode(TypedEvidenceGateMode::Enforce),
        )
    }

    /// Satisfy the status precondition each transition needs, so any refusal
    /// is the gate's rather than a precondition's.
    async fn prepare_for(&self, transition: &str) {
        self.set_status("in_review").await;
        if transition == "proposal_graduate" {
            for kind in ["scoped", "technical"] {
                self.proposals
                    .add_signoff(&self.proposal_id, kind, &self.user_id)
                    .await
                    .unwrap();
            }
            assert_eq!(
                self.proposals
                    .get(&self.proposal_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "approved",
                "the graduation precondition must hold before the gate runs"
            );
        }
    }

    async fn set_status(&self, status: &str) {
        let proposal = self
            .proposals
            .get(&self.proposal_id)
            .await
            .unwrap()
            .unwrap();
        self.proposals
            .update(
                &self.proposal_id,
                ProposalUpdateInput {
                    title: &proposal.title,
                    body: &proposal.body,
                    acceptance_criteria: &proposal.acceptance_criteria,
                    status,
                    superseded_by: None,
                    body_format: Some(&proposal.body_format),
                    event_metadata: None,
                },
            )
            .await
            .unwrap();
    }

    async fn invoke(&self, transition: &str) -> Option<String> {
        let args = match transition {
            "proposal_debate_append" => json!({
                "proposal_id": self.proposal_id,
                "kind": "verdict",
                "body": "approve: the spec is settled",
                "blocking": false,
                "agent_role": "judge",
                "against_revision_seq": 1,
                "round": 1,
            }),
            "proposal_refinement_resolve" => {
                json!({ "proposal_id": self.proposal_id, "decision": "accept" })
            }
            "proposal_signoff" => json!({ "id": self.proposal_id, "kind": "technical" }),
            "proposal_verdict_override" => json!({
                "proposal_id": self.proposal_id,
                "reason": "human accepted the residual risk",
            }),
            "proposal_graduate" => json!({ "id": self.proposal_id }),
            other => unreachable!("unknown transition {other}"),
        };
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(self.user_id.clone()), async {
                self.server().dispatch_tool(transition, args).await
            })
            .await
            .unwrap_or_else(|error| json!({ "error": error }));
        response
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Whether the transition's durable row exists in the database.
    ///
    /// This is the whole point of the matrix: the verdict is read back out of
    /// persistence, not out of the tool's return value.
    async fn persisted(&self, transition: &str) -> bool {
        match transition {
            "proposal_debate_append" => self
                .proposals
                .latest_judge_verdict(&self.proposal_id)
                .await
                .unwrap()
                .is_some_and(|verdict| verdict.body.contains("the spec is settled")),
            "proposal_signoff" => self
                .proposals
                .signoffs(&self.proposal_id)
                .await
                .unwrap()
                .iter()
                .any(|signoff| signoff.kind == "technical"),
            "proposal_verdict_override" => self
                .proposals
                .latest_verdict_override(&self.proposal_id)
                .await
                .unwrap()
                .is_some(),
            "proposal_graduate" => {
                let proposal = self
                    .proposals
                    .get(&self.proposal_id)
                    .await
                    .unwrap()
                    .unwrap();
                let breakdown = TaskRepository::new(self.db.clone(), EventBus::noop())
                    .list_by_project(&self.project_id)
                    .await
                    .unwrap()
                    .into_iter()
                    .any(|task| {
                        task.issue_type == "epic_breakdown"
                            && task.design.contains(&self.proposal_id)
                    });
                proposal.status == "building" && breakdown
            }
            // Refinement acceptance is delegated to the coordinator, whose stub
            // persists nothing. Its durable proxy is that the gate returned
            // before the delegation — captured by the error channel instead.
            _ => false,
        }
    }
}

/// Execute one fixture row against all five transitions, on a fresh proposal
/// per transition so no transition observes another's writes.
async fn run_row(harness: &Harness, transitions: &[String], row: &Row) {
    for transition in transitions {
        let case = harness.case(row).await;
        case.prepare_for(transition).await;
        let error = case.invoke(transition).await;

        if row.expects_block() {
            let finding_id = case
                .blocking_finding_id
                .clone()
                .expect("a blocking row must have an unresolved finding");
            let error =
                error.unwrap_or_else(|| panic!("{}/{transition} must be refused", row.name));
            // AC2: all four typed diagnostics in every refusal.
            for (label, needle) in [
                ("finding id", finding_id.as_str()),
                ("claim", CLAIM_QUESTION),
                ("lifecycle", row.lifecycle.as_str()),
                ("originating revision seq", "demanded against revision 1"),
            ] {
                assert!(
                    error.contains(needle),
                    "{}/{transition} refusal must name the {label}: {error}",
                    row.name
                );
            }
            assert!(
                !case.persisted(transition).await,
                "{}/{transition} was refused but still persisted its row",
                row.name
            );
        } else {
            assert!(
                !error
                    .as_deref()
                    .is_some_and(|e| e.contains("typed evidence")),
                "{}/{transition} must not be refused by the typed gate: {error:?}",
                row.name
            );
            if transition == "proposal_refinement_resolve" {
                assert_eq!(error, None, "{}/{transition} must be admitted", row.name);
            } else {
                assert!(
                    case.persisted(transition).await,
                    "{}/{transition} was admitted but persisted nothing",
                    row.name
                );
            }
        }

        // The composed gate is a production entry point in its own right, and
        // three of the five transitions reach the typed check through it.
        let proposal = case
            .proposals
            .get(&case.proposal_id)
            .await
            .unwrap()
            .unwrap();
        let composed = evaluate_composed_gate(
            &case.proposals,
            &proposal,
            &proposal.body,
            &proposal.acceptance_criteria,
            1,
            TypedEvidenceGateMode::Enforce,
        )
        .await;
        let composed_blocks = composed
            .failures()
            .iter()
            .any(|failure| failure.contains("unresolved typed evidence finding"));
        assert_eq!(
            composed_blocks,
            row.expects_block(),
            "{}/{transition}: composed gate disagreed with the row",
            row.name
        );
    }
}

/// Every fixture row, grouped so nextest runs the groups in parallel.
async fn run_group(predicate: impl Fn(&Row) -> bool) -> usize {
    let matrix = matrix();
    let rows: Vec<Row> = matrix
        .rows
        .iter()
        .filter(|row| predicate(row))
        .cloned()
        .collect();
    assert!(!rows.is_empty(), "a group must select rows");
    let harness = Harness::new().await;
    for row in &rows {
        run_row(&harness, &matrix.transitions, row).await;
    }
    rows.len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_demanded() {
    assert_eq!(run_group(|row| row.lifecycle == "demanded").await, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_spike_active() {
    assert_eq!(run_group(|row| row.lifecycle == "spike_active").await, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_evidence_received_resolved() {
    assert_eq!(
        run_group(|row| row.lifecycle == "evidence_received"
            && row.evidence_outcome.as_deref() == Some("resolved"))
        .await,
        6
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_evidence_received_partial() {
    assert_eq!(
        run_group(|row| row.lifecycle == "evidence_received"
            && row.evidence_outcome.as_deref() == Some("partial"))
        .await,
        6
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_evidence_received_unresolved() {
    assert_eq!(
        run_group(|row| row.lifecycle == "evidence_received"
            && row.evidence_outcome.as_deref() == Some("unresolved"))
        .await,
        6
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_failed() {
    assert_eq!(run_group(|row| row.lifecycle == "failed").await, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_resolved() {
    assert_eq!(run_group(|row| row.lifecycle == "resolved").await, 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_evidence_gate_matrix_withdrawn() {
    assert_eq!(run_group(|row| row.lifecycle == "withdrawn").await, 6);
}

/// One lifecycle group's membership predicate.
type RowGroup = Box<dyn Fn(&Row) -> bool>;

/// No fixture row may be silently skipped: the groups above must partition the
/// fixture exactly, and the fixture must be the full declared cross product.
#[test]
fn typed_evidence_gate_matrix_groups_cover_every_row() {
    let matrix = matrix();
    assert_eq!(matrix.transitions.len(), 5, "five gated transitions");
    let groups: [RowGroup; 8] = [
        Box::new(|row| row.lifecycle == "demanded"),
        Box::new(|row| row.lifecycle == "spike_active"),
        Box::new(|row| {
            row.lifecycle == "evidence_received"
                && row.evidence_outcome.as_deref() == Some("resolved")
        }),
        Box::new(|row| {
            row.lifecycle == "evidence_received"
                && row.evidence_outcome.as_deref() == Some("partial")
        }),
        Box::new(|row| {
            row.lifecycle == "evidence_received"
                && row.evidence_outcome.as_deref() == Some("unresolved")
        }),
        Box::new(|row| row.lifecycle == "failed"),
        Box::new(|row| row.lifecycle == "resolved"),
        Box::new(|row| row.lifecycle == "withdrawn"),
    ];
    for row in &matrix.rows {
        let matched = groups.iter().filter(|group| group(row)).count();
        assert_eq!(
            matched, 1,
            "row {} must belong to exactly one group",
            row.name
        );
    }
    assert_eq!(
        matrix.rows.len(),
        48,
        "8 lifecycles x 3 revisions x 2 verdict forms"
    );
    let names: std::collections::BTreeSet<&str> =
        matrix.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names.len(), matrix.rows.len(), "row names must be unique");
    for offset in [0, 1, 2] {
        assert_eq!(
            matrix
                .rows
                .iter()
                .filter(|row| row.revision_offset == offset)
                .count(),
            16,
            "every revision offset must be exercised across all eight lifecycles"
        );
    }
    for form in ["structured", "prose_only"] {
        assert_eq!(
            matrix
                .rows
                .iter()
                .filter(|row| row.verdict_form == form)
                .count(),
            24,
            "every verdict form must be exercised across all eight lifecycles"
        );
    }
}

/// AC3 — a withdrawal that does not carry the non-load-bearing assertion, or
/// carries an empty rationale, is refused by the repository. Such a record can
/// therefore never produce the `withdrawn` lifecycle an admitted row needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_rejects_a_non_load_bearing_withdrawal() {
    let harness = Harness::new().await;
    let row = Row {
        name: "withdrawal-boundary".into(),
        lifecycle: "evidence_received".into(),
        evidence_outcome: Some("resolved".into()),
        revision_offset: 0,
        verdict_form: "structured".into(),
        expected: "blocked".into(),
    };
    let case = harness.case(&row).await;
    let validation_id =
        djinn_db::test_support::typed_evidence_validation_snapshot_for_finding_for_test(
            &case.db,
            &case.finding_id,
        )
        .await
        .validation_id;
    let judge_task_id = case.materialize_judge().await;
    let creator = TaskRepository::new(case.db.clone(), EventBus::noop())
        .get(&judge_task_id)
        .await
        .unwrap()
        .unwrap()
        .created_by_user_id;

    for (label, rationale, non_load_bearing) in [
        ("missing assertion", "the claim no longer matters", false),
        ("empty rationale", "   ", true),
    ] {
        let error = case
            .proposals
            .dispose_evidence_atomically(AtomicEvidenceDispositionInput {
                finding_id: case.finding_id.clone(),
                validation_result_id: None,
                folding_revision: 1,
                disposition: TribunalEvidenceLifecycle::Withdrawn,
                rationale: rationale.into(),
                withdrawal_is_non_load_bearing: non_load_bearing,
                caller_user_id: creator.clone(),
            })
            .await
            .err()
            .unwrap_or_else(|| {
                panic!("{label} must be refused at the repository boundary");
            });
        assert!(
            !format!("{error}").is_empty(),
            "{label} must carry a reason"
        );
        // The refusal is not cosmetic: the finding is still unresolved, so it
        // still blocks. A bad withdrawal cannot reach an admitted row.
        assert_eq!(
            TypedEvidenceRepository::new(case.db.clone())
                .unresolved_projection(&case.proposal_id)
                .await
                .unwrap()
                .map(|projection| projection.finding_id)
                .as_deref(),
            Some(case.finding_id.as_str()),
            "{label} must leave the finding unresolved"
        );
    }

    // A correctly-formed withdrawal by the same Judge does clear it, so the
    // refusals above are the assertions' and not the fixture's.
    dispose_typed_evidence_validation_for_test(
        &case.db,
        &validation_id,
        &judge_task_id,
        TribunalEvidenceLifecycle::Withdrawn,
    )
    .await;
    assert_eq!(
        TypedEvidenceRepository::new(case.db.clone())
            .unresolved_projection(&case.proposal_id)
            .await
            .unwrap(),
        None,
        "a properly-formed withdrawal must clear the gate"
    );
}
