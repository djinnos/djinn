//! Contract tests for `board_*` MCP tools.
//!
//! Only `board_health` migrated — it only needs DB-backed tasks/notes.  The
//! `board_reconcile` test stays in `djinn-server` because it requires the
//! real coordinator and slot-pool actors (our harness stubs those).

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_db::LivenessEvidenceSnapshot;
use djinn_db::LivenessRepository;
use djinn_db::{EpicRepository, ProposalCreateInput, ProposalRepository, TaskRepository};
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
            session_id: session.id.clone(),
            task_id: Some(task.id.clone()),
            task_run_id: None,
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
            session_id: session.id.clone(),
            task_id: Some(task.id.clone()),
            task_run_id: None,
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
            session_id: session.id.clone(),
            task_id: Some(task.id.clone()),
            task_run_id: None,
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
            session_id: session.id.clone(),
            task_id: Some(task.id.clone()),
            task_run_id: None,
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
    assert_eq!(
        gate.get("gate_verdict").and_then(|v| v.as_str()),
        Some("stranded"),
        "gate_verdict must be stranded for an unblocked task"
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
