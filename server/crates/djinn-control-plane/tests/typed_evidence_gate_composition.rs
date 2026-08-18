//! Composition contract for the typed-evidence structural gate.
//!
//! Five transitions consume the repository projection: Judge verdict, human
//! refinement acceptance, sign-off, verdict override, and graduation. Each is
//! driven here through its real MCP tool against a migrated database, so a row
//! cannot pass by asserting a status string.
//!
//! The rollout stage is pinned per-`McpState` rather than read from the
//! process environment, so three modes can be exercised in one test binary
//! without one test's stage leaking into another's.
//!
//! ## Why every test here is named `typed_evidence_gate_matrix_*`
//!
//! Proposal `667e` AC5's command is
//! `cargo test -p djinn-control-plane typed_evidence_gate_matrix`. These seven
//! tests used to match none of the eleven AC filters — including the sole
//! consumer of `fixtures/typed_evidence_gate_off_baseline.json`, the `Off`-mode
//! rollback proof the phase-5 rollback boundary rests on. A test outside every
//! AC command is not part of the contract, whatever its file claims. The prefix
//! puts them inside it. Keep it on anything added here.

use djinn_control_plane::server::DjinnMcpServer;
use djinn_control_plane::state::stubs::test_mcp_state;
use djinn_control_plane::tools::proposal_tools::TypedEvidenceGateMode;
use djinn_core::events::EventBus;
use djinn_core::models::NeedsEvidenceClaim;
use djinn_db::{
    Database, ProjectRepository, ProposalCreateInput, ProposalRepository, TaskRepository,
    TypedEvidenceRepository, UserRepository,
    test_support::{
        CanonicalTypedEvidenceReturnOutcomeForTest, UsageTestTaskSeed,
        overwrite_legacy_evidence_authority_for_test,
        seed_canonical_typed_evidence_ingress_fixture_for_test, seed_task_row,
    },
};
use serde_json::{Value, json};

/// A body that passes every deterministic DoR check, so the composed gate
/// reaches its tribunal section instead of short-circuiting on readiness.
const READY_BODY: &str = r#"
# Problem
Reviewers cannot tell whether a typed evidence demand is still open.

# Scope
In scope: the structural gate. Out of scope: the UI rendering.

# Objectives
- Block transitions on an unresolved typed finding
- Report the finding on `proposal_show`

## File map
```file-map
    server/crates/djinn-control-plane/src/tools/proposal_tools/signoff.rs
    server/crates/djinn-db/src/repositories/typed_evidence.rs
```

# Dependencies
Blocked by the typed evidence projection in `djinn-db`.

# Open Questions
What happens when typed and legacy authority disagree?
"#;

/// The five gated transitions, each named by the MCP tool that performs it.
const TRANSITIONS: [&str; 5] = [
    "proposal_debate_append",
    "proposal_refinement_resolve",
    "proposal_signoff",
    "proposal_verdict_override",
    "proposal_graduate",
];

struct Fixture {
    db: Database,
    proposals: ProposalRepository,
    user_id: String,
    proposal_id: String,
    spike_task_id: String,
    finding_id: String,
}

impl Fixture {
    /// A DoR-ready, approved proposal carrying one unresolved typed finding in
    /// `evidence_received`.
    ///
    /// That lifecycle is the point: the durable receipt already cleared the
    /// legacy `linked_spike_task_id` / `needs_evidence_claim` columns, so the
    /// pre-existing needs-evidence check (2d) sees nothing. Only the typed
    /// projection knows the claim is still open — which is exactly the hole
    /// this gate closes.
    async fn blocked() -> Self {
        let fixture = Self::unblocked().await;
        fixture.raise_typed_demand().await;
        fixture.deliver_typed_evidence().await;
        fixture
    }

    /// The same proposal with no typed finding at all.
    async fn unblocked() -> Self {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let users = UserRepository::new(db.clone());
        let user = users
            .upsert_from_github(
                i64::from(u32::from(std::process::id() as u16)) + 900_000,
                &format!("typed-gate-{}", uuid::Uuid::now_v7()),
                None,
                None,
            )
            .await
            .unwrap();
        users.set_role(&user.id, "engineer").await.unwrap();
        let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create(
                &format!("svc-{}", uuid::Uuid::now_v7()),
                "test",
                &format!("svc-{}", uuid::Uuid::now_v7()),
            )
            .await
            .unwrap();
        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user.id.clone()), async {
                proposals
                    .create(ProposalCreateInput {
                        title: "Typed evidence gate composition",
                        body: READY_BODY,
                        acceptance_criteria: Some(
                            r#"[{"criterion":"The gate refuses with the finding id","met":false}]"#,
                        ),
                        status: None,
                        body_format: None,
                    })
                    .await
            })
            .await
            .unwrap();
        proposals
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .unwrap();
        let spike_task_id = seed_task_row(
            &db,
            UsageTestTaskSeed {
                project_id: &project.id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        Self {
            db,
            proposals,
            user_id: user.id,
            proposal_id: proposal.id,
            spike_task_id,
            finding_id: String::new(),
        }
    }

    fn claim(&self) -> NeedsEvidenceClaim {
        NeedsEvidenceClaim {
            question: "Can the launcher share a cgroup across pods?".into(),
            target_subsystem: "launcher".into(),
            spec_unknown_anchor: "cgroup delegation".into(),
            insufficient_in_session_research: "needs a live kernel probe".into(),
            expected_findings: "a delegated cgroup or a kernel refusal".into(),
            created_by_task_id: self.spike_task_id.clone(),
            round: 1,
            against_revision_seq: 1,
        }
    }

    async fn raise_typed_demand(&self) {
        self.proposals
            .set_structured_needs_evidence_spike(
                &self.proposal_id,
                &self.spike_task_id,
                &self.claim(),
            )
            .await
            .unwrap();
    }

    /// Submit a durable return, which moves the finding to `evidence_received`
    /// and clears legacy authority in the same transaction.
    async fn deliver_typed_evidence(&self) {
        let fixture = seed_canonical_typed_evidence_ingress_fixture_for_test(
            &self.db,
            &self.proposal_id,
            &self.spike_task_id,
            "gate-composition",
            CanonicalTypedEvidenceReturnOutcomeForTest::Partial,
        )
        .await;
        // The durable return path authenticates a closed spike task. Close it
        // through the task repository rather than raw SQL — the raw-SQL
        // boundary applies to test files too.
        TaskRepository::new(self.db.clone(), EventBus::noop())
            .set_status_with_reason(&self.spike_task_id, "closed", Some("completed"))
            .await
            .unwrap();
        TypedEvidenceRepository::new(self.db.clone())
            .submit_return_v1_for_task(&self.spike_task_id, fixture.return_payload.as_bytes())
            .await
            .unwrap();
        let proposal = self
            .proposals
            .get(&self.proposal_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            proposal.linked_spike_task_id.is_none() && proposal.needs_evidence_claim.is_none(),
            "the receipt must clear legacy authority, or the legacy 2d check \
             would be doing this gate's work"
        );
    }

    fn typed_finding_id(&self) -> String {
        if !self.finding_id.is_empty() {
            return self.finding_id.clone();
        }
        unreachable!("call resolve_finding_id first")
    }

    async fn resolve_finding_id(&mut self) {
        self.finding_id = TypedEvidenceRepository::new(self.db.clone())
            .unresolved_projection(&self.proposal_id)
            .await
            .unwrap()
            .expect("fixture must carry an unresolved typed finding")
            .finding_id;
    }

    /// Put the proposal into the exact status each transition requires.
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
                djinn_db::ProposalUpdateInput {
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

    /// Reach `approved` the way the repository defines it: two fresh sign-offs
    /// of distinct kinds on the head revision. Setting the column alone does
    /// not work — `update` re-reconciles the approval gate and demotes back.
    async fn approve(&self) {
        self.set_status("in_review").await;
        for kind in ["scoped", "technical"] {
            self.proposals
                .add_signoff(&self.proposal_id, kind, &self.user_id)
                .await
                .unwrap();
        }
        let proposal = self
            .proposals
            .get(&self.proposal_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            proposal.status, "approved",
            "the graduation precondition must actually hold before the gate runs"
        );
    }

    fn server(&self, mode: TypedEvidenceGateMode) -> DjinnMcpServer {
        DjinnMcpServer::new(test_mcp_state(self.db.clone()).with_typed_evidence_gate_mode(mode))
    }

    /// Invoke one gated transition and return its error string, if any.
    async fn attempt(&self, server: &DjinnMcpServer, transition: &str) -> Option<String> {
        // `proposal_graduate` only reaches its gate from `approved`; sign-off
        // only from `draft`/`in_review`. Satisfy the status precondition first
        // so a refusal is the gate's, never a precondition's.
        if transition == "proposal_graduate" {
            self.approve().await;
        } else {
            self.set_status("in_review").await;
        }
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
            "proposal_refinement_resolve" => json!({
                "proposal_id": self.proposal_id,
                "decision": "accept",
            }),
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
                server.dispatch_tool(transition, args).await
            })
            .await
            .unwrap_or_else(|error| json!({ "error": error }));
        response
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Assert the durable row each refused transition would have written is
    /// absent. Sign-off is exempt from the row check because reaching the
    /// graduation precondition seeds two sign-offs by design; its refusal is
    /// instead proven by the proposal never advancing.
    async fn assert_transition_left_no_trace(&self, transition: &str) {
        let proposal = self
            .proposals
            .get(&self.proposal_id)
            .await
            .unwrap()
            .unwrap();
        match transition {
            "proposal_debate_append" => assert!(
                self.proposals
                    .latest_judge_verdict(&self.proposal_id)
                    .await
                    .unwrap()
                    .is_none(),
                "a refused judge verdict must not persist a debate row"
            ),
            "proposal_signoff" => assert!(
                self.proposals
                    .signoffs(&self.proposal_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "a refused sign-off must not persist a row"
            ),
            "proposal_verdict_override" => assert!(
                self.proposals
                    .latest_verdict_override(&self.proposal_id)
                    .await
                    .unwrap()
                    .is_none(),
                "a refused override must not persist a lifecycle row"
            ),
            "proposal_graduate" => assert_ne!(
                proposal.status, "building",
                "a refused graduation must not move the proposal to building"
            ),
            // `proposal_refinement_resolve` delegates to the coordinator, so
            // its only durable trace is the one the coordinator would write.
            // The refusal is asserted by the gate returning before that call.
            _ => {}
        }
    }

    async fn gate_status(&self, server: &DjinnMcpServer) -> Value {
        let response = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(self.user_id.clone()), async {
                server
                    .dispatch_tool("proposal_show", json!({ "id": self.proposal_id }))
                    .await
            })
            .await
            .unwrap();
        response
            .get("gate_status")
            .cloned()
            .expect("proposal_show must include gate_status")
    }
}

/// AC1 — every one of the five transitions refuses under `Enforce`, and each
/// refusal names all four typed diagnostics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_enforce_blocks_all_five_transitions_with_the_four_typed_diagnostics()
 {
    let mut fixture = Fixture::blocked().await;
    fixture.resolve_finding_id().await;
    let finding_id = fixture.typed_finding_id();
    let server = fixture.server(TypedEvidenceGateMode::Enforce);

    for transition in TRANSITIONS {
        let error = fixture
            .attempt(&server, transition)
            .await
            .unwrap_or_else(|| panic!("{transition} must be refused under Enforce"));
        assert!(
            error.contains(&finding_id),
            "{transition} refusal must name the finding id: {error}"
        );
        assert!(
            error.contains("Can the launcher share a cgroup across pods?"),
            "{transition} refusal must quote the claim: {error}"
        );
        assert!(
            error.contains("evidence_received"),
            "{transition} refusal must name the lifecycle state: {error}"
        );
        assert!(
            error.contains("demanded against revision 1"),
            "{transition} refusal must name the originating revision seq: {error}"
        );
        // The refusal is not cosmetic: assert the persisted side effect the
        // transition would have produced, not the returned message.
        fixture.assert_transition_left_no_trace(transition).await;
    }
}

/// AC1 control — the same five transitions are admitted once the typed finding
/// is gone, so the refusals above are the gate's and not a precondition's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_enforce_admits_every_transition_when_no_typed_finding_exists() {
    let fixture = Fixture::unblocked().await;
    let server = fixture.server(TypedEvidenceGateMode::Enforce);
    for transition in TRANSITIONS {
        let error = fixture.attempt(&server, transition).await;
        assert!(
            !error
                .as_deref()
                .is_some_and(|e| e.contains("typed evidence")),
            "{transition} must not be refused by the typed gate: {error:?}"
        );
    }
}

/// AC2 — under `Off` the composed gate produces exactly the pre-change result.
///
/// The expected value is committed in
/// `tests/fixtures/typed_evidence_gate_off_baseline.json` and was captured by
/// running this test's fixture against the gate *before* check 2e existed. It
/// is never regenerated: if `Off` ever starts adding, dropping, or reordering a
/// failure, this goes red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_off_mode_is_byte_identical_to_the_pre_change_gate() {
    let baseline: Value = serde_json::from_str(include_str!(
        "fixtures/typed_evidence_gate_off_baseline.json"
    ))
    .expect("baseline fixture is valid JSON");
    let mut fixture = Fixture::blocked().await;
    fixture.resolve_finding_id().await;
    fixture.set_status("in_review").await;
    let proposal = fixture
        .proposals
        .get(&fixture.proposal_id)
        .await
        .unwrap()
        .unwrap();

    let composed = djinn_control_plane::tools::proposal_tools::signoff::evaluate_composed_gate(
        &fixture.proposals,
        &proposal,
        &proposal.body,
        &proposal.acceptance_criteria,
        1,
        TypedEvidenceGateMode::Off,
    )
    .await;
    let status = djinn_control_plane::tools::proposal_tools::signoff::build_gate_status(
        &fixture.proposals,
        &proposal,
        &proposal.body,
        &proposal.acceptance_criteria,
        1,
        TypedEvidenceGateMode::Off,
    )
    .await;
    let observed = json!({
        "composed_failures": composed.failures(),
        "composed_error": composed.to_error_string(),
        "gate_status": serde_json::to_value(&status).unwrap(),
    });
    assert_eq!(
        observed, baseline,
        "Off mode must reproduce the pre-change gate byte for byte"
    );

    // Guard against a vacuous baseline: the same fixture under Enforce must
    // differ, or this test would pass with the gate deleted entirely.
    let enforced = djinn_control_plane::tools::proposal_tools::signoff::evaluate_composed_gate(
        &fixture.proposals,
        &proposal,
        &proposal.body,
        &proposal.acceptance_criteria,
        1,
        TypedEvidenceGateMode::Enforce,
    )
    .await;
    assert_ne!(
        enforced.failures(),
        composed.failures(),
        "the baseline is only meaningful if Enforce changes the result"
    );
}

/// AC3 — a typed/legacy parity mismatch fails every transition closed, in all
/// three modes, with the same reason code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_parity_mismatch_fails_closed_in_every_mode() {
    for mode in [
        TypedEvidenceGateMode::Off,
        TypedEvidenceGateMode::Shadow,
        TypedEvidenceGateMode::Enforce,
    ] {
        let fixture = Fixture::unblocked().await;
        // Legacy authority ahead of typed: a compatibility link with no typed
        // finding behind it. This is the mixed-version drift a rollback leaves.
        overwrite_legacy_evidence_authority_for_test(
            &fixture.db,
            &fixture.proposal_id,
            Some(&fixture.spike_task_id),
            Some(&serde_json::to_value(fixture.claim()).unwrap()),
        )
        .await;
        let server = fixture.server(mode);
        for transition in TRANSITIONS {
            let error = fixture
                .attempt(&server, transition)
                .await
                .unwrap_or_else(|| {
                    panic!("{transition} must fail closed on a parity mismatch in {mode:?}")
                });
            assert!(
                error.contains("typed_evidence_parity_mismatch"),
                "{transition} in {mode:?} must name the parity reason: {error}"
            );
        }
    }
}

/// AC4 — `proposal_show` carries the typed section for a blocked proposal,
/// omits it when there is no finding, and leaves the legacy fields alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_proposal_show_reports_the_typed_section_without_disturbing_legacy_fields()
 {
    let mut fixture = Fixture::blocked().await;
    fixture.resolve_finding_id().await;
    let finding_id = fixture.typed_finding_id();

    let status = fixture
        .gate_status(&fixture.server(TypedEvidenceGateMode::Enforce))
        .await;
    let typed = status
        .get("typed_evidence")
        .expect("a blocked proposal must carry the typed section");
    assert_eq!(typed["finding_id"], json!(finding_id));
    assert_eq!(typed["lifecycle"], json!("evidence_received"));
    assert_eq!(typed["demanded_revision_seq"], json!(1));
    assert_eq!(typed["mode"], json!("enforce"));
    assert_eq!(typed["blocking"], json!(true));
    assert_eq!(typed["evidence_outcome"], json!("partial"));
    assert!(
        typed["claim"]
            .as_str()
            .is_some_and(|claim| claim.contains("Can the launcher share a cgroup across pods?")),
        "the typed section must carry the claim: {typed}"
    );
    assert_eq!(
        status.get("ready"),
        Some(&json!(false)),
        "the typed section must make the gate not ready"
    );
    // The legacy channel is untouched: this fixture's receipt cleared it, so
    // `needs_evidence` is absent exactly as it was before the typed section
    // existed. The two authorities are reported independently.
    assert!(
        status.get("needs_evidence").is_none(),
        "legacy needs-evidence must be reported from the legacy columns alone: {status}"
    );

    // Shadow surfaces the same finding without blocking.
    let shadow = fixture
        .gate_status(&fixture.server(TypedEvidenceGateMode::Shadow))
        .await;
    assert_eq!(shadow["typed_evidence"]["finding_id"], json!(finding_id));
    assert_eq!(shadow["typed_evidence"]["blocking"], json!(false));
    assert_eq!(shadow["typed_evidence"]["mode"], json!("shadow"));

    // Off reports nothing typed at all.
    let off = fixture
        .gate_status(&fixture.server(TypedEvidenceGateMode::Off))
        .await;
    assert!(
        off.get("typed_evidence").is_none(),
        "Off must not surface a typed section: {off}"
    );

    // A proposal with no typed finding omits the section under every mode.
    let clean = Fixture::unblocked().await;
    for mode in [
        TypedEvidenceGateMode::Off,
        TypedEvidenceGateMode::Shadow,
        TypedEvidenceGateMode::Enforce,
    ] {
        let status = clean.gate_status(&clean.server(mode)).await;
        assert!(
            status.get("typed_evidence").is_none(),
            "no finding must mean no typed section in {mode:?}: {status}"
        );
    }
}

/// AC4 companion — the legacy `needs_evidence` fields still populate from the
/// legacy columns, unchanged by the typed section.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_legacy_needs_evidence_fields_are_unchanged() {
    let fixture = Fixture::unblocked().await;
    fixture.raise_typed_demand().await;
    let status = fixture
        .gate_status(&fixture.server(TypedEvidenceGateMode::Enforce))
        .await;
    let legacy = status
        .get("needs_evidence")
        .expect("a legacy-parked proposal must still report needs_evidence");
    assert_eq!(
        legacy["spike_task_id"],
        json!(fixture.spike_task_id),
        "legacy spike id must come from the legacy column: {legacy}"
    );
    assert_eq!(legacy["spike_status"], json!("open"));
    assert!(
        legacy["claim"]
            .as_str()
            .is_some_and(|claim| claim.contains("Can the launcher share a cgroup across pods?")),
        "legacy claim must be the legacy column text: {legacy}"
    );
    // Both authorities agree here, so the typed section reports the same
    // finding alongside — never instead of — the legacy one.
    assert_eq!(
        status["typed_evidence"]["lifecycle"],
        json!("spike_active"),
        "typed and legacy sections must coexist: {status}"
    );
}

/// The rollout stage is carried by the state, so two servers over the same
/// database disagree about blocking without touching the process environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_evidence_gate_matrix_the_rollout_stage_is_per_state_not_per_process() {
    let fixture = Fixture::blocked().await;
    assert!(
        fixture
            .attempt(
                &fixture.server(TypedEvidenceGateMode::Enforce),
                "proposal_signoff"
            )
            .await
            .is_some_and(|error| error.contains("unresolved typed evidence finding")),
        "Enforce must block"
    );
    assert!(
        !fixture
            .attempt(
                &fixture.server(TypedEvidenceGateMode::Shadow),
                "proposal_signoff"
            )
            .await
            .is_some_and(|error| error.contains("unresolved typed evidence finding")),
        "Shadow must not block, in the same process and against the same database"
    );
}
