//! Contract tests for `board_*` MCP tools.
//!
//! Only `board_health` migrated — it only needs DB-backed tasks/notes.  The
//! `board_reconcile` test stays in `djinn-server` because it requires the
//! real coordinator and slot-pool actors (our harness stubs those).

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_db::LivenessEvidenceSnapshot;
use djinn_db::LivenessRepository;
use djinn_db::repositories::user::UserRepository;
use djinn_db::{EpicRepository, ProposalCreateInput, ProposalRepository, TaskRepository};
use djinn_provider::repos::CredentialRepository;
use serde_json::json;

#[tokio::test]
async fn board_health_with_no_pool_returns_response_shape() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    // Backward-compatible coarse status fields must remain present.
    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
    assert_eq!(
        response
            .get("refinement_phantom_active_count")
            .and_then(|value| value.as_i64()),
        Some(0),
        "a clean board has a deterministic phantom count"
    );
    assert_eq!(
        response
            .get("refinement_phantom_reaps_24h")
            .and_then(|value| value.as_i64()),
        Some(0),
        "a clean board has a deterministic durable-reap count"
    );
    // Memory health is no longer embedded in board_health (the planner workflow that consumed it was removed with proposal 1omc); note-health
    // signals live on the dedicated `memory_health` tool.
    assert!(response.get("memory_health").is_none());
}

#[tokio::test]
async fn board_health_returns_additive_liveness_and_stranded_sections() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    // New additive sections produced by the DB-side board_health work in
    // task lke3 — the MCP surface must surface them with default/skip-empty
    // behavior so old DB payloads that pre-date these sections still
    // deserialize (verified implicitly here because the harness has a
    // brand-new DB with no rows, yet the call succeeds).
    let liveness_outcomes = response
        .get("liveness_outcomes")
        .expect("liveness_outcomes section must be present");
    assert_eq!(
        liveness_outcomes.get("total").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert!(liveness_outcomes.get("by_verdict").is_some());
    assert!(
        liveness_outcomes
            .get("recent")
            .and_then(|v| v.as_array())
            .is_some()
    );

    let protocol_violations = response
        .get("protocol_violations")
        .expect("protocol_violations section must be present");
    assert_eq!(
        protocol_violations.get("total").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert!(
        protocol_violations
            .get("recent")
            .and_then(|v| v.as_array())
            .is_some()
    );

    let stranded_ready = response
        .get("stranded_ready")
        .expect("stranded_ready section must be present");
    assert_eq!(
        stranded_ready.get("total").and_then(|v| v.as_i64()),
        Some(0)
    );
    // Base 30-minute threshold from the design contract must be echoed back
    // so clients can interpret severity without hard-coding the ladder.
    assert_eq!(
        stranded_ready
            .get("threshold_minutes")
            .and_then(|v| v.as_i64()),
        Some(30)
    );
    assert!(
        stranded_ready
            .get("findings")
            .and_then(|v| v.as_array())
            .is_some()
    );

    // The coarse status fields still coexist with the additive sections.
    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
}

/// Consistency test: liveness evidence seeded via `LivenessRepository` (the
/// jk7v DB contract) must surface through the `board_health` MCP tool's
/// `liveness_outcomes` section with matching task id, session id, verdict,
/// and outcome fields.
#[tokio::test]
async fn board_health_liveness_outcomes_match_seeded_evidence() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;
    let session = common::create_test_session(harness.db(), &project.id, &task.id).await;

    // Seed liveness evidence with a dead verdict / dead_reclaimed outcome.
    let liveness_repo = LivenessRepository::new(harness.db().clone());
    let evidence_id = liveness_repo
        .persist_evidence(&LivenessEvidenceSnapshot {
            session_id: Some(session.id.clone()),
            task_id: Some(task.id.clone()),
            task_run_id: None,
            trigger_identity: None,
            verdict: "dead".to_owned(),
            outcome_kind: Some("dead_reclaimed".to_owned()),
            outcome_reason: Some("hard_runtime_exceeded".to_owned()),
            evidence: serde_json::json!({
                "pod_phase": "Succeeded",
                "claim_ttl_expired": true,
            }),
        })
        .await
        .expect("persist liveness evidence");
    assert!(!evidence_id.is_empty());

    // Call board_health through the MCP tool.
    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    let liveness_outcomes = response
        .get("liveness_outcomes")
        .expect("liveness_outcomes section must be present");
    assert_eq!(
        liveness_outcomes.get("total").and_then(|v| v.as_i64()),
        Some(1),
        "must surface exactly 1 liveness outcome"
    );

    // by_verdict must include dead: 1.
    let by_verdict = liveness_outcomes
        .get("by_verdict")
        .expect("by_verdict must be present");
    assert_eq!(
        by_verdict.get("dead").and_then(|v| v.as_i64()),
        Some(1),
        "by_verdict must count 1 dead verdict"
    );

    // recent must contain our evidence row.
    let recent = liveness_outcomes
        .get("recent")
        .and_then(|v| v.as_array())
        .expect("recent must be an array");
    let item = recent
        .iter()
        .find(|i| {
            i.get("task_id").and_then(|v| v.as_str()) == Some(&task.id)
                && i.get("session_id").and_then(|v| v.as_str()) == Some(&session.id)
        })
        .expect("liveness outcome for our task/session must be present");

    assert_eq!(
        item.get("verdict").and_then(|v| v.as_str()),
        Some("dead"),
        "verdict must match seeded evidence"
    );
    assert_eq!(
        item.get("outcome_kind").and_then(|v| v.as_str()),
        Some("dead_reclaimed"),
        "outcome_kind must match seeded evidence"
    );
    assert_eq!(
        item.get("outcome_reason").and_then(|v| v.as_str()),
        Some("hard_runtime_exceeded"),
        "outcome_reason must match seeded evidence"
    );

    // Also seed a protocol-violation evidence row and verify it surfaces in
    // the protocol_violations section with matching fields.
    let _evidence_id_2 = liveness_repo
        .persist_evidence(&LivenessEvidenceSnapshot {
            session_id: Some(session.id.clone()),
            task_id: Some(task.id.clone()),
            task_run_id: None,
            trigger_identity: None,
            verdict: "protocol_violation".to_owned(),
            outcome_kind: Some("protocol_violation".to_owned()),
            outcome_reason: None,
            evidence: serde_json::json!({
                "reason": "unexpected_message_type",
            }),
        })
        .await
        .expect("persist protocol violation evidence");

    // Re-query board_health.
    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch after protocol violation seed");

    let protocol_violations = response
        .get("protocol_violations")
        .expect("protocol_violations section must be present");
    assert!(
        protocol_violations
            .get("total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 1,
        "must surface at least 1 protocol violation"
    );

    let pv_recent = protocol_violations
        .get("recent")
        .and_then(|v| v.as_array())
        .expect("recent must be an array");
    let pv_item = pv_recent
        .iter()
        .find(|i| {
            i.get("task_id").and_then(|v| v.as_str()) == Some(&task.id)
                && i.get("session_id").and_then(|v| v.as_str()) == Some(&session.id)
        })
        .expect("protocol violation for our task/session must be present");

    assert_eq!(
        pv_item.get("verdict").and_then(|v| v.as_str()),
        Some("protocol_violation"),
        "protocol violation verdict must match"
    );
    assert_eq!(
        pv_item.get("outcome_kind").and_then(|v| v.as_str()),
        Some("protocol_violation"),
        "protocol violation outcome_kind must match"
    );
}

/// Comprehensive backward-compatibility regression through the MCP surface:
/// legacy coarse fields (stale_tasks, epic_stats, review_queue,
/// stale_threshold_hours) must remain present and deserializable alongside
/// all additive liveness_outcomes, protocol_violations, stranded_ready, and
/// dispatch-gate sections.
///
/// Seeds all three categories of additive data and verifies every field
/// coexists in a single MCP board_health response.
#[tokio::test]
async fn board_health_mcp_legacy_and_additive_fields_coexist() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;
    let session = common::create_test_session(harness.db(), &project.id, &task.id).await;

    // ── Seed liveness evidence (dead verdict) ──────────────────────────
    let liveness_repo = LivenessRepository::new(harness.db().clone());
    let _evidence_id = liveness_repo
        .persist_evidence(&LivenessEvidenceSnapshot {
            session_id: Some(session.id.clone()),
            task_id: Some(task.id.clone()),
            task_run_id: None,
            trigger_identity: None,
            verdict: "dead".to_owned(),
            outcome_kind: Some("dead_reclaimed".to_owned()),
            outcome_reason: Some("hard_runtime_exceeded".to_owned()),
            evidence: serde_json::json!({
                "pod_phase": "Succeeded",
                "claim_ttl_expired": true,
            }),
        })
        .await
        .expect("persist dead liveness evidence");

    // ── Seed protocol-violation evidence ───────────────────────────────
    let _pv_evidence_id = liveness_repo
        .persist_evidence(&LivenessEvidenceSnapshot {
            session_id: Some(session.id.clone()),
            task_id: Some(task.id.clone()),
            task_run_id: None,
            trigger_identity: None,
            verdict: "protocol_violation".to_owned(),
            outcome_kind: Some("protocol_violation".to_owned()),
            outcome_reason: None,
            evidence: serde_json::json!({
                "reason": "unexpected_message_type",
            }),
        })
        .await
        .expect("persist protocol violation evidence");

    // ── Seed stranded-ready task (separate from liveness task) ──────────
    let stranded_task = common::create_test_task(harness.db(), &project.id, &epic.id).await;
    djinn_db::test_support::backdate_task_updated_at(harness.db(), &stranded_task.id, "90 minutes")
        .await;

    // ── Single MCP board_health call ───────────────────────────────────
    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    // ── Legacy coarse fields must remain present ───────────────────────
    assert!(
        response.get("stale_tasks").is_some(),
        "legacy stale_tasks must remain present"
    );
    assert!(
        response.get("epic_stats").is_some(),
        "legacy epic_stats must remain present"
    );
    assert!(
        response.get("review_queue").is_some(),
        "legacy review_queue must remain present"
    );
    assert!(
        response.get("stale_threshold_hours").is_some(),
        "legacy stale_threshold_hours must remain present"
    );

    // ── Additive liveness_outcomes ─────────────────────────────────────
    let liveness_outcomes = response
        .get("liveness_outcomes")
        .expect("liveness_outcomes section must be present");
    assert!(
        liveness_outcomes
            .get("total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 2,
        "must surface at least 2 liveness outcomes"
    );
    assert!(liveness_outcomes.get("by_verdict").is_some());
    let recent = liveness_outcomes
        .get("recent")
        .and_then(|v| v.as_array())
        .expect("recent must be an array");
    // Verify classifier outcome/evidence fields on the dead verdict item.
    let dead_item = recent
        .iter()
        .find(|i| {
            i.get("task_id").and_then(|v| v.as_str()) == Some(&task.id)
                && i.get("verdict").and_then(|v| v.as_str()) == Some("dead")
        })
        .expect("dead verdict for task must be present");
    assert_eq!(
        dead_item.get("outcome_kind").and_then(|v| v.as_str()),
        Some("dead_reclaimed"),
        "classifier outcome_kind must be present on the MCP surface"
    );
    assert_eq!(
        dead_item.get("outcome_reason").and_then(|v| v.as_str()),
        Some("hard_runtime_exceeded"),
        "classifier outcome_reason must be present on the MCP surface"
    );

    // ── Additive protocol_violations ───────────────────────────────────
    let protocol_violations = response
        .get("protocol_violations")
        .expect("protocol_violations section must be present");
    assert!(
        protocol_violations
            .get("total")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 1,
        "must surface at least 1 protocol violation"
    );

    // ── Additive stranded_ready ────────────────────────────────────────
    let stranded_ready = response
        .get("stranded_ready")
        .expect("stranded_ready section must be present");
    assert_eq!(
        stranded_ready
            .get("threshold_minutes")
            .and_then(|v| v.as_i64()),
        Some(30),
        "must echo the base 30-minute threshold"
    );
    let findings = stranded_ready
        .get("findings")
        .and_then(|v| v.as_array())
        .expect("findings must be an array");
    let stranded_finding = findings
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(&stranded_task.id))
        .expect("stranded_task must appear in findings");
    assert_eq!(
        stranded_finding.get("severity").and_then(|v| v.as_str()),
        Some("error"),
        "90-minute backdate must produce error severity"
    );
    // Dispatch-gate evidence must be present with expected fields.
    let gate = stranded_finding
        .get("dispatch_gate")
        .expect("dispatch_gate must be present on stranded finding");
    assert!(gate.get("evaluated_role").is_some());
    assert!(gate.get("gate_verdict").is_some());
    assert!(gate.get("breaker_open").is_some());
    assert!(gate.get("manually_paused").is_some());
    assert!(gate.get("rate_limited").is_some());
    assert!(gate.get("credential_available").is_some());
    assert!(gate.get("reasons").is_some());
    // The Kueue admission projection must survive the TYPED round-trip — the
    // DB builds this JSON, `BoardHealthResponse` parses it, and the tool
    // re-serializes it. A field the type does not model is dropped here rather
    // than at some later, quieter moment.
    let kueue = gate
        .get("kueue_admission")
        .expect("kueue_admission must reach the MCP payload");
    assert_eq!(
        kueue.get("projection_state").and_then(|v| v.as_str()),
        Some("no_workloads_observed"),
        "an unarmed test cluster reports the INERT variant, never a pending queue"
    );
    assert_eq!(kueue.get("pending").and_then(|v| v.as_i64()), Some(0));
    assert!(
        gate.get("kueue_workload").is_none(),
        "a task with no Workload carries no per-task Kueue block"
    );
    let unevaluated = gate
        .get("coverage")
        .and_then(|c| c.get("unevaluated_gates"))
        .and_then(|v| v.as_array())
        .expect("coverage.unevaluated_gates must be present");
    assert!(
        unevaluated.contains(&json!("kueue_clusterqueue_admission")),
        "an empty projection cannot distinguish an unarmed cluster from a dead \
         reflector, so the gate stays unevaluated; got {unevaluated:?}"
    );
    // Threshold ladder.
    let threshold = stranded_finding
        .get("threshold")
        .expect("threshold must be present");
    assert_eq!(
        threshold.get("warning_minutes").and_then(|v| v.as_i64()),
        Some(30)
    );
    assert_eq!(
        threshold.get("error_minutes").and_then(|v| v.as_i64()),
        Some(60)
    );
    assert_eq!(
        threshold.get("critical_minutes").and_then(|v| v.as_i64()),
        Some(180)
    );
}

/// Consistency test: a stranded-ready task surfaced by `board_health` must
/// carry the expected severity, threshold ladder, and dispatch-gate evidence
/// matching the jk7v DB contract.
#[tokio::test]
async fn board_health_stranded_ready_matches_seeded_task() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;

    // This fixture represents a dispatch-eligible task, not a legacy
    // creator-less row. Persist both its attributed user and an active private
    // credential so the dispatch-gate evidence remains credential-available.
    let user = UserRepository::new(harness.db().clone())
        .upsert_from_github(999_003, "board-health-stranded-test", None, None)
        .await
        .expect("create attributed task user");
    TaskRepository::new(harness.db().clone(), EventBus::noop())
        .set_created_by_user_id(&task.id, &user.id)
        .await
        .expect("attribute stranded task creator");
    CredentialRepository::new(harness.db().clone(), EventBus::noop())
        .set_with_owner(
            "anthropic",
            "ANTHROPIC_API_KEY",
            "sk-board-health-test",
            Some(&user.id),
        )
        .await
        .expect("create attributed task credential");

    // Backdate the task's updated_at well past the 30-minute threshold.
    djinn_db::test_support::backdate_task_updated_at(harness.db(), &task.id, "90 minutes").await;

    // Call board_health through the MCP tool.
    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    let stranded_ready = response
        .get("stranded_ready")
        .expect("stranded_ready section must be present");

    assert_eq!(
        stranded_ready
            .get("threshold_minutes")
            .and_then(|v| v.as_i64()),
        Some(30),
        "must echo the base 30-minute threshold"
    );

    let findings = stranded_ready
        .get("findings")
        .and_then(|v| v.as_array())
        .expect("findings must be an array");
    let finding = findings
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(&task.id))
        .expect("stranded_ready must contain our backdated task");

    // Severity: 90 minutes → error (>= 60m, < 180m).
    assert_eq!(
        finding.get("severity").and_then(|v| v.as_str()),
        Some("error"),
        "90-minute backdate must produce error severity"
    );

    // Elapsed minutes must be >= 60 (allowing for DB clock skew).
    let elapsed = finding
        .get("elapsed_minutes")
        .and_then(|v| v.as_i64())
        .expect("elapsed_minutes must be present");
    assert!(
        elapsed >= 60,
        "elapsed must be at least 60 minutes, got {elapsed}"
    );

    // Threshold ladder must be present and match the design contract.
    let threshold = finding.get("threshold").expect("threshold must be present");
    assert_eq!(
        threshold.get("warning_minutes").and_then(|v| v.as_i64()),
        Some(30),
        "warning_minutes must be 30"
    );
    assert_eq!(
        threshold.get("error_minutes").and_then(|v| v.as_i64()),
        Some(60),
        "error_minutes must be 60"
    );
    assert_eq!(
        threshold.get("critical_minutes").and_then(|v| v.as_i64()),
        Some(180),
        "critical_minutes must be 180"
    );

    // Dispatch-gate evidence must be present with expected fields.
    let gate = finding
        .get("dispatch_gate")
        .expect("dispatch_gate must be present");
    // No gate board_health can evaluate fired, and the verdict must say
    // exactly that rather than assert the task is merely `stranded` — that
    // label was emitted whenever `reasons` was empty, which for a task with no
    // chosen model was structurally guaranteed.
    assert_eq!(
        gate.get("gate_verdict").and_then(|v| v.as_str()),
        Some("unexplained"),
        "gate_verdict must be `unexplained` when no evaluated gate fired"
    );
    let coverage = gate
        .get("coverage")
        .expect("an unexplained verdict must ship its coverage");
    assert_eq!(
        coverage.get("scope").and_then(|v| v.as_str()),
        Some("partial")
    );
    assert!(
        coverage
            .get("unevaluated_gates")
            .and_then(|v| v.as_array())
            .is_some_and(|gates| !gates.is_empty()),
        "coverage must name the dispatcher gates this section did not consult"
    );
    assert_eq!(
        gate.get("credential_available").and_then(|v| v.as_bool()),
        Some(true),
        "attributed task credential must be available"
    );
    assert_eq!(
        gate.get("evaluated_role").and_then(|v| v.as_str()),
        Some("worker"),
        "evaluated_role must be worker for a default task"
    );
    assert_eq!(
        gate.get("breaker_open").and_then(|v| v.as_bool()),
        Some(false),
        "breaker_open must be false for a fresh task"
    );
    assert_eq!(
        gate.get("manually_paused").and_then(|v| v.as_bool()),
        Some(false),
        "manually_paused must be false for a fresh task"
    );
    assert_eq!(
        gate.get("rate_limited").and_then(|v| v.as_bool()),
        Some(false),
        "rate_limited must be false for a fresh task"
    );
}

/// Closed-parent orphan section is surfaced through the MCP board_health tool
/// with the additive default behavior: present, empty when no drift, and
/// deserializable into the typed response struct.
#[tokio::test]
async fn board_health_closed_parent_open_children_empty_by_default() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    let section = response
        .get("closed_parent_open_children")
        .expect("closed_parent_open_children section must be present");
    assert_eq!(section.get("total").and_then(|v| v.as_i64()), Some(0));
    assert!(section.get("findings").and_then(|v| v.as_array()).is_some());
}

/// End-to-end regression: `board_health` reports closed-parent orphan findings
/// through the MCP surface with the complete snapshot schema expected by the
/// repair path: status, terminal parent ids, exclusion evidence, and the shared
/// disposition row.
#[tokio::test]
async fn board_health_closed_parent_open_children_reports_populated_findings() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());
    let proposals = ProposalRepository::new(harness.db().clone(), common::test_events());

    // Ready orphan: should close with parent_closed.
    let ready_epic = common::create_test_epic(harness.db(), &project.id).await;
    let ready = common::create_test_task(harness.db(), &project.id, &ready_epic.id).await;
    epics
        .set_status_raw(&ready_epic.id, "closed")
        .await
        .unwrap();

    // In-flight orphan: should park with historical_parent_closed_in_flight.
    let flight_epic = common::create_test_epic(harness.db(), &project.id).await;
    let flight = common::create_test_task(harness.db(), &project.id, &flight_epic.id).await;
    tasks.set_status(&flight.id, "in_progress").await.unwrap();
    let session = common::create_test_session(harness.db(), &project.id, &flight.id).await;
    epics
        .set_status_raw(&flight_epic.id, "closed")
        .await
        .unwrap();

    // PR-active orphan: should park with historical_parent_closed_pr_active.
    let pr_epic = common::create_test_epic(harness.db(), &project.id).await;
    let pr = common::create_test_task(harness.db(), &project.id, &pr_epic.id).await;
    tasks.set_status(&pr.id, "pr_review").await.unwrap();
    tasks
        .set_pr_url(&pr.id, "https://github.com/djinnos/djinn/pull/999999")
        .await
        .unwrap();
    epics.set_status_raw(&pr_epic.id, "closed").await.unwrap();

    // Guarded orphan: another open proposal parent keeps it retained.
    let guard_epic = common::create_test_epic(harness.db(), &project.id).await;
    let guard = common::create_test_task(harness.db(), &project.id, &guard_epic.id).await;
    epics
        .set_status_raw(&guard_epic.id, "closed")
        .await
        .unwrap();
    let live_proposal = proposals
        .create(ProposalCreateInput {
            title: "live parent",
            body: "",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .unwrap();
    proposals
        .link_epic(&live_proposal.id, &guard_epic.id, &project.id)
        .await
        .unwrap();

    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    let section = response
        .get("closed_parent_open_children")
        .expect("closed_parent_open_children section must be present");
    assert_eq!(section.get("total").and_then(|v| v.as_i64()), Some(4));
    let findings = section
        .get("findings")
        .and_then(|v| v.as_array())
        .expect("findings must be an array");
    assert_eq!(findings.len(), 4);

    let mut actual = std::collections::BTreeMap::new();
    for f in findings {
        let id = f.get("id").and_then(|v| v.as_str()).unwrap().to_owned();
        let action = f
            .get("recommended_action")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let reason = f
            .get("recommended_reason")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_owned();
        let other_open: Vec<String> = f
            .get("other_open_parent_ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let external: Vec<String> = f
            .get("external_open_dependents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.get("task_id").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        actual.insert(id, (action, reason, other_open, external));
    }

    // Ready orphan: close / parent_closed.
    let (a, r, o, e) = actual.get(&ready.id).unwrap();
    assert_eq!(a, "close");
    assert_eq!(r, "parent_closed");
    assert!(o.is_empty() && e.is_empty());

    // In-flight orphan: park with session retained in evidence.
    let (a, r, o, e) = actual.get(&flight.id).unwrap();
    assert_eq!(a, "park");
    assert_eq!(r, "historical_parent_closed_in_flight");
    assert!(o.is_empty() && e.is_empty());
    let flight_finding = findings
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(&flight.id))
        .unwrap();
    assert_eq!(
        flight_finding
            .get("preserved_session_id")
            .and_then(|v| v.as_str()),
        Some(session.id.as_str())
    );

    // PR-active orphan: park with PR URL retained in evidence.
    let (a, r, o, e) = actual.get(&pr.id).unwrap();
    assert_eq!(a, "park");
    assert_eq!(r, "historical_parent_closed_pr_active");
    assert!(o.is_empty() && e.is_empty());
    let pr_finding = findings
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(&pr.id))
        .unwrap();
    assert_eq!(
        pr_finding.get("preserved_pr_url").and_then(|v| v.as_str()),
        Some("https://github.com/djinnos/djinn/pull/999999")
    );

    // Guarded orphan: retain because another open proposal parent exists.
    let (a, r, o, _e) = actual.get(&guard.id).unwrap();
    assert_eq!(a, "retain");
    assert_eq!(r, "other_open_parent");
    assert_eq!(o.len(), 1);
    assert_eq!(o[0], live_proposal.id);
}

/// **The MCP boundary is load-bearing and must be pinned.**
///
/// `board_health_impl` does `serde_json::from_value::<BoardHealthResponse>(report)`
/// and re-serializes the parsed struct. `BoardHealthStrandedReadyFinding` has no
/// `#[serde(flatten)]` catch-all, so any field the type does not model is
/// **silently dropped** on the way out. The djinn-db section can be perfectly
/// correct and the operator still gets nothing.
///
/// That matters specifically for `gate_escalation`. A finding carrying it is a
/// task with a *live* dispatch gate — a cooldown deadline in the future — which
/// this section would ordinarily exclude. Drop the escalation block and an
/// operator sees a critical stranded finding for a task whose `dispatch_gate`
/// says `breaker_open: true`, with nothing at all explaining why a gated task
/// is being reported. That is the same "the alarm was silenced one layer out"
/// failure this whole change exists to remove, moved to the last hop.
///
/// Reverting `BoardHealthStrandedReadyFinding::gate_escalation` (or
/// `BoardHealthStrandedReady::gate_exclusion_bound_minutes`) leaves every
/// djinn-db and doctor test green. This test is the only thing that fails.
#[tokio::test]
async fn board_health_mcp_surface_preserves_gate_escalation_evidence() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;

    // Attribute the task to a creator with an ACTIVE credential, so the only
    // gate suppressing this finding is the breaker cooldown. Without this the
    // owner-credential gate would fire too and `overridden_gates` would carry
    // two entries, which would weaken the assertion below into a `contains`.
    let user = UserRepository::new(harness.db().clone())
        .upsert_from_github(999_007, "board-health-escalation-test", None, None)
        .await
        .expect("create attributed task user");
    TaskRepository::new(harness.db().clone(), EventBus::noop())
        .set_created_by_user_id(&task.id, &user.id)
        .await
        .expect("attribute escalated task creator");
    CredentialRepository::new(harness.db().clone(), EventBus::noop())
        .set_with_owner(
            "anthropic",
            "ANTHROPIC_API_KEY",
            "sk-board-health-escalation-test",
            Some(&user.id),
        )
        .await
        .expect("create attributed task credential");

    // Four days of strand — the 2026-08-12 → 2026-08-16 window.
    djinn_db::test_support::backdate_task_updated_at(harness.db(), &task.id, "5760 minutes").await;

    // The exact `dispatch_state` shape the breaker-open path leaves behind: a
    // cooldown deadline at the ~30-minute ladder ceiling, an inflight model,
    // and `failure_streak = 0` — that path does not advance the streak.
    sqlx::query(
        "INSERT INTO dispatch_state \
             (task_id, failure_streak, cooldown_until, last_dispatched_role, inflight_model_id) \
         VALUES ($1, 0, now() AT TIME ZONE 'utc' + interval '30 minutes', \
                 'worker', 'openai/gpt-5.6-terra')",
    )
    .bind(&task.id)
    .execute(harness.db().pool())
    .await
    .expect("seed breaker cooldown dispatch_state");

    // Call the REAL MCP tool — this is the round trip under test.
    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    let stranded_ready = response
        .get("stranded_ready")
        .expect("stranded_ready section must be present");

    // The section-level bound must survive the round trip, or a client cannot
    // interpret an escalation without hard-coding the number.
    assert_eq!(
        stranded_ready
            .get("gate_exclusion_bound_minutes")
            .and_then(|v| v.as_i64()),
        Some(180),
        "the MCP surface must echo the gate-exclusion bound it applied; \
         stranded_ready was {stranded_ready}"
    );

    let finding = stranded_ready
        .get("findings")
        .and_then(|v| v.as_array())
        .expect("findings must be an array")
        .iter()
        .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(&task.id))
        .unwrap_or_else(|| {
            panic!(
                "a task suppressed by a breaker cooldown for 5760 minutes must reach the MCP \
                 surface; stranded_ready was {stranded_ready}"
            )
        });

    // The gate is live. That is precisely why the escalation has to be here:
    // without it this finding is inexplicable.
    assert_eq!(
        finding
            .pointer("/dispatch_gate/breaker_open")
            .and_then(|v| v.as_bool()),
        Some(true),
        "the finding is reported despite a live breaker cooldown"
    );

    let escalation = finding.get("gate_escalation").unwrap_or_else(|| {
        panic!("gate_escalation must survive the MCP round trip; finding was {finding}")
    });
    assert!(
        !escalation.is_null(),
        "gate_escalation must not be nulled out by the MCP round trip; finding was {finding}"
    );

    // Contents, not mere presence.
    assert_eq!(
        escalation.get("escalated").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        escalation.get("overridden_gates"),
        Some(&json!(["breaker_cooldown"])),
        "the overridden gate identity must survive intact, not be flattened away"
    );
    assert_eq!(
        escalation.get("bound_minutes").and_then(|v| v.as_i64()),
        Some(180)
    );
    assert_eq!(
        escalation.get("bound_multiple").and_then(|v| v.as_i64()),
        Some(6)
    );

    let suppressed = escalation
        .get("suppressed_minutes")
        .and_then(|v| v.as_i64())
        .expect("suppressed_minutes must survive the round trip");
    assert!(
        suppressed >= 5_760,
        "suppressed_minutes must carry the real strand duration, got {suppressed}"
    );
    assert_eq!(
        Some(suppressed),
        finding.get("elapsed_minutes").and_then(|v| v.as_i64()),
        "suppressed_minutes and elapsed_minutes are the SAME clock and must stay equal \
         across the round trip"
    );

    // The row evidence an operator needs to act: which model, and which deadline.
    assert_eq!(
        escalation
            .pointer("/evidence/inflight_model_id")
            .and_then(|v| v.as_str()),
        Some("openai/gpt-5.6-terra"),
        "the evidence block must survive with the model that was hard-disabled"
    );
    assert_eq!(
        escalation
            .pointer("/evidence/failure_streak")
            .and_then(|v| v.as_i64()),
        Some(0),
        "the breaker-open path does not advance the streak; the evidence must say so"
    );
    assert_eq!(
        escalation
            .pointer("/evidence/last_dispatched_role")
            .and_then(|v| v.as_str()),
        Some("worker")
    );
    assert!(
        escalation
            .pointer("/evidence/cooldown_until")
            .and_then(|v| v.as_str())
            .is_some_and(|cd| cd.ends_with('Z')),
        "the deadline that was suppressing the finding must survive: {escalation}"
    );

    let summary = escalation
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        summary.contains("breaker cooldown") && summary.contains("openai/gpt-5.6-terra"),
        "an operator must read the gate and the model in one line from the MCP payload: \
         {summary}"
    );

    // The machine-readable reason must survive alongside the human-readable one.
    assert!(
        finding
            .pointer("/dispatch_gate/reasons")
            .and_then(|v| v.as_array())
            .is_some_and(|r| r.contains(&json!("breaker_cooldown_sustained_past_bound"))),
        "dispatch_gate.reasons must carry the escalation reason across the round trip: \
         {finding}"
    );
}
