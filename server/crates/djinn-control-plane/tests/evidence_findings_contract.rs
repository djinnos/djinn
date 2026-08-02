//! Black-box EvidenceCompletionV1 and Judge projection contracts.

use djinn_control_plane::tools::{
    evidence_command::{ServerCommandObservation, record_command_observation},
    evidence_findings::render_evidence_judge_projection,
    evidence_plan::{
        EvidenceMethod, EvidencePlanCapture, EvidencePlanCheckInput, EvidencePlanIdentity,
        capture_evidence_plan,
    },
};
use djinn_core::models::NeedsEvidenceClaim;
use djinn_db::{
    CreateTaskAttemptParams, EvidenceRepository, ProposalCreateInput, ProposalRepository,
    SessionRepository, TaskAttemptRepository, TaskRepository,
    repositories::session::CreateSessionParams,
    test_support::structured_evidence_handoff_counts_for_test,
};
use djinn_slot::{finalize_handlers::handle_submit_work, test_helpers};
use serde::Deserialize;

const COMMIT: &str = "captured-contract-commit";
const WORKTREE: &str = "captured-contract-worktree";

#[derive(Debug, Deserialize)]
struct ContractFixture {
    valid_cases: Vec<ValidCase>,
    rejections: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ValidCase {
    name: String,
    observation: Option<Observation>,
    command_finding: bool,
    #[serde(default)]
    omit_all_findings: bool,
    expected_outcome: String,
    expected_command_health: String,
    expected_gaps: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Observation {
    exit_code: Option<i32>,
    signal: Option<i32>,
    runner_failure: Option<String>,
    #[serde(default)]
    timed_out: bool,
}

fn cases() -> ContractFixture {
    serde_json::from_str(include_str!("fixtures/evidence_findings_v1_cases.json"))
        .expect("evidence findings fixture is valid")
}

struct Harness {
    db: djinn_db::Database,
    ctx: djinn_slot::SlotContext,
    task_id: String,
    session_id: String,
    proposal_id: String,
    plan_id: String,
    invocation_id: Option<String>,
}

async fn harness(observation: Option<&Observation>) -> Harness {
    let fixture = test_helpers::seed_context_fixture().await;
    let session = SessionRepository::new(fixture.db.clone(), fixture.ctx.event_bus.clone())
        .create(CreateSessionParams {
            project_id: &fixture.project.id,
            task_id: Some(&fixture.task.id),
            model: "contract-model",
            agent_type: "architect",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create authenticated evidence session");
    let proposals = ProposalRepository::new(fixture.db.clone(), fixture.ctx.event_bus.clone());
    let proposal = proposals
        .create(ProposalCreateInput {
            title: "Evidence completion contract",
            body: "Frozen contract fixture",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        })
        .await
        .expect("create linked proposal");
    proposals
        .set_structured_needs_evidence_spike(
            &proposal.id,
            &fixture.task.id,
            &NeedsEvidenceClaim {
                question: "Does structured evidence resolve the claim?".into(),
                target_subsystem: "evidence finalization".into(),
                spec_unknown_anchor: "EvidenceCompletionV1".into(),
                insufficient_in_session_research: "requires frozen provenance".into(),
                expected_findings: "code, graph, and command anchors".into(),
                round: 2,
                against_revision_seq: 3,
                created_by_task_id: fixture.task.id.clone(),
            },
        )
        .await
        .expect("link evidence spike");
    let identity = EvidencePlanIdentity {
        spike_task_id: fixture.task.id.clone(),
        session_id: session.id.clone(),
        captured_commit_sha: COMMIT.into(),
        worktree_fingerprint: WORKTREE.into(),
    };
    let evidence = EvidenceRepository::new(fixture.db.clone());
    let plan_id = capture_evidence_plan(
        &evidence,
        identity.clone(),
        EvidencePlanCapture {
            checks: vec![
                EvidencePlanCheckInput {
                    check_id: "code".into(),
                    question: "Does the source contain the seam?".into(),
                    method: EvidenceMethod::Code,
                },
                EvidencePlanCheckInput {
                    check_id: "graph".into(),
                    question: "Does the graph resolve the seam?".into(),
                    method: EvidenceMethod::Graph,
                },
                EvidencePlanCheckInput {
                    check_id: "command".into(),
                    question: "Does the focused command pass?".into(),
                    method: EvidenceMethod::Command,
                },
            ],
        },
    )
    .await
    .expect("capture frozen plan");
    let invocation_id = if let Some(observation) = observation {
        Some(
            record_command_observation(
                &evidence,
                &identity,
                "command",
                ServerCommandObservation {
                    argv: vec![
                        "cargo".into(),
                        "test".into(),
                        "-p".into(),
                        "djinn-control-plane".into(),
                        "evidence_findings_contract".into(),
                    ],
                    canonical_cwd: "/workspace/djinn/server".into(),
                    launch_state: if observation.runner_failure.is_some() {
                        "failed_to_launch"
                    } else {
                        "launched"
                    }
                    .into(),
                    process_state: if observation.timed_out {
                        "timed_out"
                    } else if observation.signal.is_some() {
                        "signaled"
                    } else if observation.runner_failure.is_some() {
                        "runner_failed"
                    } else {
                        "exited"
                    }
                    .into(),
                    launched_at: Some("2026-01-02T03:04:05Z".into()),
                    finished_at: Some("2026-01-02T03:04:06Z".into()),
                    exit_code: observation.exit_code,
                    signal: observation.signal,
                    runner_failure: observation.runner_failure.clone(),
                    elapsed_millis: Some(1000),
                    timeout_millis: Some(30000),
                    timed_out: observation.timed_out,
                    stdout_digest: Some("sha256:stdout-contract-digest".into()),
                    stdout_excerpt: Some("contract stdout excerpt".into()),
                    stdout_truncated: true,
                    stderr_digest: Some("sha256:stderr-contract-digest".into()),
                    stderr_excerpt: Some("contract stderr excerpt".into()),
                    stderr_truncated: false,
                },
            )
            .await
            .expect("record immutable invocation")
            .id,
        )
    } else {
        None
    };
    Harness {
        db: fixture.db,
        ctx: fixture.ctx,
        task_id: fixture.task.id,
        session_id: session.id,
        proposal_id: proposal.id,
        plan_id,
        invocation_id,
    }
}

fn completion_payload(
    h: &Harness,
    command_finding: bool,
    omit_all_findings: bool,
) -> serde_json::Value {
    let mut findings = if omit_all_findings {
        Vec::new()
    } else {
        vec![
            serde_json::json!({
                "check_id": "code",
                "summary": "the production submit seam owns finalization",
                "anchor": {"kind":"code","path":"server/crates/djinn-slot/src/finalize_handlers.rs","start_line":112,"end_line":169,"captured_commit_sha":COMMIT}
            }),
            serde_json::json!({
                "check_id": "graph",
                "summary": "the finalizer resolves through the control-plane node",
                "anchor": {"kind":"graph","node_id":"rust:handle_submit_work","graph_identity":COMMIT}
            }),
        ]
    };
    if command_finding {
        findings.push(serde_json::json!({
            "check_id": "command",
            "summary": "the immutable command event supplies process health",
            "anchor": {"kind":"command","invocation":{"invocation_id":h.invocation_id.as_deref().expect("command case has invocation")}}
        }));
    }
    serde_json::json!({
        "task_id": h.task_id,
        "commit_title": "record evidence contract",
        "summary": "structured evidence submitted",
        "files_changed": [],
        "remaining_concerns": [],
        "evidence_completion": {
            "schema_version": 1,
            "plan_id": h.plan_id,
            "terminal_results": [
                {"check_id":"code","method":"code","terminal":true},
                {"check_id":"graph","method":"graph","terminal":true},
                {"check_id":"command","method":"command","terminal":true}
            ],
            "findings": findings
        }
    })
}

async fn submit_case(case: &ValidCase) -> (Harness, serde_json::Value, String) {
    let h = harness(case.observation.as_ref()).await;
    let payload = completion_payload(&h, case.command_finding, case.omit_all_findings);
    assert!(
        handle_submit_work(&payload, &h.task_id, &h.session_id, &h.ctx).await,
        "valid case {}",
        case.name
    );
    let hydration = EvidenceRepository::new(h.db.clone())
        .hydrate_by_identity(&h.task_id, &h.session_id)
        .await
        .expect("hydrate projection")
        .expect("frozen plan exists");
    let projection = hydration
        .finalized_projection
        .expect("valid submission finalizes projection")
        .payload;
    let rendered = render_evidence_judge_projection(&projection).expect("render Judge projection");
    (h, projection, rendered)
}

#[tokio::test]
async fn evidence_findings_contract() {
    let fixture = cases();
    for case in &fixture.valid_cases {
        let (h, projection, rendered) = submit_case(case).await;
        assert_eq!(
            projection["outcome"], case.expected_outcome,
            "{}",
            case.name
        );
        let command = projection["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["check_id"] == "command")
            .unwrap();
        assert_eq!(
            command["health"], case.expected_command_health,
            "{}",
            case.name
        );
        assert_eq!(
            serde_json::from_value::<Vec<String>>(projection["gaps"].clone()).unwrap(),
            case.expected_gaps,
            "{}",
            case.name
        );
        assert!(rendered.contains(&format!("Evidence completion: {}", case.expected_outcome)));
        let counts =
            structured_evidence_handoff_counts_for_test(&h.db, &h.plan_id, &h.proposal_id).await;
        assert_eq!(
            (counts.finalized_projections, counts.compatibility_debates),
            (1, 1)
        );
    }

    let h = harness(Some(&Observation {
        exit_code: Some(0),
        ..Default::default()
    }))
    .await;
    let base = completion_payload(&h, true, false);
    let attempt_repo = TaskAttemptRepository::new(h.db.clone());
    let attempt = attempt_repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: "019fa49d-ffff-7000-8000-000000000001",
            task_id: &h.task_id,
            role: "worker",
            dispatch_key: "evidence-findings-rejection-contract",
            session_id: Some(&h.session_id),
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed pending lifecycle attempt");
    let baseline_attempt = serde_json::to_value(&attempt).expect("serialize attempt baseline");
    let baseline_activity = TaskRepository::new(h.db.clone(), h.ctx.event_bus.clone())
        .list_activity(&h.task_id)
        .await
        .unwrap()
        .len();
    let baseline_proposal = serde_json::to_value(
        ProposalRepository::new(h.db.clone(), h.ctx.event_bus.clone())
            .get(&h.proposal_id)
            .await
            .unwrap()
            .unwrap(),
    )
    .expect("serialize proposal lifecycle baseline");
    for rejection in fixture.rejections {
        let mut payload = base.clone();
        let mut session = h.session_id.as_str();
        match rejection.as_str() {
            "unknown_completion_field" => payload["evidence_completion"]["unknown"] = true.into(),
            "unknown_terminal_result_field" => {
                payload["evidence_completion"]["terminal_results"][0]["unknown"] = true.into()
            }
            "unknown_finding_field" => {
                payload["evidence_completion"]["findings"][0]["unknown"] = true.into()
            }
            "unknown_anchor_field" => {
                payload["evidence_completion"]["findings"][0]["anchor"]["unknown"] = true.into()
            }
            "unknown_invocation_field" => {
                payload["evidence_completion"]["findings"][2]["anchor"]["invocation"]["unknown"] =
                    true.into()
            }
            "prose_only_finding" => {
                payload["evidence_completion"]["findings"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("anchor");
            }
            "code_method_mismatch" => {
                payload["evidence_completion"]["findings"][0]["check_id"] = "graph".into()
            }
            "graph_method_mismatch" => {
                payload["evidence_completion"]["findings"][1]["check_id"] = "code".into()
            }
            "command_method_mismatch" => {
                payload["evidence_completion"]["findings"][2]["check_id"] = "code".into()
            }
            "unresolvable_code_anchor" => {
                payload["evidence_completion"]["findings"][0]["anchor"]["captured_commit_sha"] =
                    "other".into()
            }
            "unresolvable_graph_anchor" => {
                payload["evidence_completion"]["findings"][1]["anchor"]["graph_identity"] =
                    "other".into()
            }
            "unresolvable_command_anchor" => {
                payload["evidence_completion"]["findings"][2]["anchor"]["invocation"]["invocation_id"] =
                    "invented".into()
            }
            "caller_owned_health" => {
                payload["evidence_completion"]["findings"][2]["anchor"]["invocation"]["health"] =
                    "ok".into()
            }
            "caller_owned_event_state" => {
                payload["evidence_completion"]["findings"][2]["anchor"]["invocation"]["exit_code"] =
                    0.into()
            }
            "duplicate_check" => {
                payload["evidence_completion"]["terminal_results"][1]["check_id"] = "code".into()
            }
            "omitted_check" => {
                payload["evidence_completion"]["terminal_results"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
            "extra_check" => payload["evidence_completion"]["terminal_results"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"check_id":"extra","method":"code","terminal":true})),
            "wrong_plan_identity" => {
                payload["evidence_completion"]["plan_id"] = "other-plan".into()
            }
            "wrong_authenticated_session" => session = "other-authenticated-session",
            other => panic!("unknown rejection fixture {other}"),
        }
        assert!(
            !handle_submit_work(&payload, &h.task_id, session, &h.ctx).await,
            "{rejection}"
        );
        let counts =
            structured_evidence_handoff_counts_for_test(&h.db, &h.plan_id, &h.proposal_id).await;
        assert_eq!(
            (counts.finalized_projections, counts.compatibility_debates),
            (0, 0),
            "{rejection}"
        );
        let activities = TaskRepository::new(h.db.clone(), h.ctx.event_bus.clone())
            .list_activity(&h.task_id)
            .await
            .unwrap();
        assert_eq!(activities.len(), baseline_activity, "{rejection}");
        let proposal = ProposalRepository::new(h.db.clone(), h.ctx.event_bus.clone())
            .get(&h.proposal_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(proposal).unwrap(),
            baseline_proposal,
            "proposal lifecycle changed for {rejection}"
        );
        let persisted_attempt = attempt_repo
            .get(&attempt.id)
            .await
            .unwrap()
            .expect("pending attempt remains present");
        assert_eq!(
            serde_json::to_value(persisted_attempt).unwrap(),
            baseline_attempt,
            "attempt lifecycle changed for {rejection}"
        );
    }
}

#[tokio::test]
async fn evidence_judge_projection_contract() {
    let fixture = cases();
    for snapshot_case in ["resolved", "partial_timeout", "unresolved"] {
        let case = fixture
            .valid_cases
            .iter()
            .find(|case| case.name == snapshot_case)
            .unwrap();
        let (h, projection, mut rendered) = submit_case(case).await;
        if let Some(invocation_id) = h.invocation_id {
            rendered = rendered.replace(&invocation_id, "<invocation-id>");
        }
        for check_id in ["code", "graph", "command"] {
            assert!(
                rendered.contains(&format!("check {check_id} ")),
                "missing {check_id}"
            );
        }
        for gap in projection["gaps"].as_array().unwrap() {
            assert!(rendered.contains(gap.as_str().unwrap()));
        }
        if snapshot_case == "resolved" || snapshot_case == "partial_timeout" {
            for required in [
                h.plan_id.as_str(),
                "captured-contract-commit",
                "captured-contract-worktree",
                "rust:handle_submit_work",
                "cargo",
                "test",
                "/workspace/djinn/server",
                "launch_state",
                "process_state",
                "launched_at",
                "2026-01-02T03:04:05Z",
                "finished_at",
                "2026-01-02T03:04:06Z",
                "exit_code",
                "signal",
                "runner_failure",
                "elapsed_millis",
                "1000",
                "timeout_millis",
                "30000",
                "timed_out",
                "sha256:stdout-contract-digest",
                "contract stdout excerpt",
                "stdout_truncated",
                "sha256:stderr-contract-digest",
                "contract stderr excerpt",
                "stderr_truncated",
            ] {
                assert!(
                    rendered.contains(required),
                    "Judge projection omitted {required}"
                );
            }
        } else {
            assert!(rendered.contains("not positive evidence"));
            assert!(
                !rendered.contains("finding "),
                "unanchored prose must not render positively"
            );
        }
        rendered = rendered.replace(&h.plan_id, "<plan-id>");
        insta::assert_snapshot!(format!("evidence_judge_{snapshot_case}"), rendered);
    }
}

/// Profile rollback is deliberately a tool-surface operation. This persists the
/// complete production evidence hand-off first, records its typed lifecycle
/// receipt, then rehydrates every durable artifact without any migration or
/// storage rewrite.
#[tokio::test]
async fn evidence_artifacts_remain_readable_after_advertisement_rollback() {
    let case = cases()
        .valid_cases
        .into_iter()
        .find(|case| case.name == "resolved")
        .expect("resolved fixture exists");
    let (h, projection, rendered_before) = submit_case(&case).await;
    let evidence = EvidenceRepository::new(h.db.clone());

    let persisted_before = evidence
        .hydrate_by_identity(&h.task_id, &h.session_id)
        .await
        .expect("hydrate frozen artifacts")
        .expect("frozen artifacts exist");
    assert_eq!(persisted_before.plan.id, h.plan_id);
    assert_eq!(persisted_before.plan.checks.len(), 3, "frozen plan checks");
    assert_eq!(
        persisted_before.invocations.len(),
        1,
        "immutable invocation"
    );
    assert_eq!(
        persisted_before
            .finalized_projection
            .as_ref()
            .expect("structured findings projection")
            .payload,
        projection
    );
    assert!(rendered_before.contains("finding code:"));
    assert!(rendered_before.contains("finding graph:"));
    assert!(rendered_before.contains("finding command:"));

    // The same completed spike drives the production lifecycle receipt writer.
    // No evidence row is rewritten while the receipt is persisted.
    TaskRepository::new(h.db.clone(), h.ctx.event_bus.clone())
        .set_status_with_reason(&h.task_id, "closed", Some("completed"))
        .await
        .expect("close completed evidence spike");
    let proposals = ProposalRepository::new(h.db.clone(), h.ctx.event_bus.clone());
    let outcome = proposals
        .persist_terminal_linked_spike_evidence_lifecycle(
            &h.proposal_id,
            &h.task_id,
            "closed",
            Some("completed"),
        )
        .await
        .expect("persist typed lifecycle receipt");
    assert!(matches!(
        outcome,
        djinn_db::repositories::proposal::TerminalLinkedEvidenceSpikeOutcome::EvidenceReceived {
            derived_outcome: Some(
                djinn_db::repositories::proposal::EvidenceDerivedOutcome::Resolved
            )
        }
    ));

    // The profile rollback test disables only `evidence_plan`/`evidence_exec`
    // advertisement. Rehydration must therefore observe byte-for-byte the same
    // persisted artifacts after that configuration-only operation.
    let persisted_after = evidence
        .hydrate_by_identity(&h.task_id, &h.session_id)
        .await
        .expect("rehydrate after advertisement rollback")
        .expect("frozen artifacts survive rollback");
    assert_eq!(
        serde_json::to_value(&persisted_after.plan).expect("serialize plan"),
        serde_json::to_value(&persisted_before.plan).expect("serialize baseline plan")
    );
    assert_eq!(
        serde_json::to_value(&persisted_after.invocations).expect("serialize invocations"),
        serde_json::to_value(&persisted_before.invocations)
            .expect("serialize baseline invocations")
    );
    assert_eq!(
        persisted_after
            .finalized_projection
            .expect("finalized projection remains readable")
            .payload,
        projection
    );
    assert_eq!(
        render_evidence_judge_projection(&projection).expect("rehydrate Judge projection"),
        rendered_before
    );

    let receipt = proposals
        .revisions(&h.proposal_id)
        .await
        .expect("read lifecycle receipt")
        .into_iter()
        .find(|revision| revision.event_kind == "refinement_evidence_received")
        .expect("typed receipt exists");
    let metadata =
        djinn_db::repositories::proposal::EvidenceLifecycleMetadata::parse_event_metadata(
            receipt.event_metadata.as_deref(),
        )
        .expect("parse typed receipt")
        .expect("receipt metadata exists");
    assert_eq!(
        metadata.derived_outcome,
        Some(djinn_db::repositories::proposal::EvidenceDerivedOutcome::Resolved)
    );
}
