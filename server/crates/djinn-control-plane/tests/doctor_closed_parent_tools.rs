//! MCP integration coverage for closed-parent orphan doctor repair.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::doctor::registry;
use djinn_db::DoctorFindingRepository;
use serde_json::json;

async fn doctor_test_harness() -> McpTestHarness {
    let harness = McpTestHarness::new().await;
    djinn_db::test_support::ensure_doctor_findings_schema(harness.db()).await;
    harness
}

// ---------------------------------------------------------------------------
// closed_parent_open_children: persisted dry-run contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_db_dry_run_is_read_only() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, ClosedParentOpenChildrenSource,
        TaskRepositoryClosedParentOpenChildrenSource, register_closed_parent_open_children_check,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, ProposalCreateInput, ProposalRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());
    let mut ids = Vec::new();
    for status in ["open", "in_progress", "pr_review", "open"] {
        let epic = common::create_test_epic(harness.db(), &project.id).await;
        let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;
        tasks.set_status(&task.id, status).await.unwrap();
        if status == "pr_review" {
            tasks
                .set_pr_url(&task.id, "https://github.com/djinnos/djinn/pull/999999")
                .await
                .unwrap();
        }
        epics.set_status_raw(&epic.id, "closed").await.unwrap();
        ids.push(task.id);
    }
    let proposals = ProposalRepository::new(harness.db().clone(), common::test_events());
    let proposal = proposals
        .create(ProposalCreateInput {
            title: "live parent",
            body: "",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .unwrap();
    let guarded_task = tasks.get(&ids[3]).await.unwrap().unwrap();
    proposals
        .link_epic(
            &proposal.id,
            guarded_task.epic_id.as_deref().unwrap(),
            &project.id,
        )
        .await
        .unwrap();
    // Establish that the real production query sees every fixture before the
    // named check is registered. This prevents stale registry/source state
    // from turning the MCP assertion into an in-memory-only regression.
    let health = tasks.board_health(30).await.unwrap();
    let health_findings = health["closed_parent_open_children"]["findings"]
        .as_array()
        .unwrap();
    assert_eq!(health_findings.len(), 4, "board health: {health}");
    let mut expected = std::collections::BTreeMap::new();
    expected.insert(ids[0].as_str(), ("close", "parent_closed"));
    expected.insert(
        ids[1].as_str(),
        ("park", "historical_parent_closed_in_flight"),
    );
    expected.insert(
        ids[2].as_str(),
        ("park", "historical_parent_closed_pr_active"),
    );
    expected.insert(ids[3].as_str(), ("retain", "other_open_parent"));
    for finding in health_findings {
        let task_id = finding["id"].as_str().unwrap();
        let (action, guard) = expected
            .get(task_id)
            .unwrap_or_else(|| panic!("unexpected board-health task {task_id}"));
        assert_eq!(finding["recommended_disposition"]["action"], *action);
        assert_eq!(finding["recommended_disposition"]["guard"], *guard);
    }
    let mut tasks_before = Vec::new();
    let mut activity_before = Vec::new();
    for id in &ids {
        tasks_before.push(serde_json::to_value(tasks.get(id).await.unwrap().unwrap()).unwrap());
        activity_before.push(serde_json::to_value(tasks.list_activity(id).await.unwrap()).unwrap());
    }
    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    assert_eq!(source.snapshot()["findings"], json!(health_findings));
    register_closed_parent_open_children_check(registry(), source);
    let response = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    assert_eq!(response["ok"], true);
    let findings = response["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 4);
    let repo = DoctorFindingRepository::new(harness.db().clone());
    let mut actual = std::collections::BTreeMap::new();
    for result in findings {
        let finding = repo
            .get(result["finding_id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        let owner = finding.entity_ids["task_id"].as_str().unwrap();
        assert_eq!(finding.entity_ids["task_id"], owner);
        assert_eq!(finding.evidence["board_health_finding"]["id"], owner);
        assert_eq!(
            finding.resolver_snapshot.as_ref().unwrap()["inputs"]["board_health_finding"]["id"],
            owner
        );
        let health_finding = health_findings
            .iter()
            .find(|health| health["id"].as_str() == Some(owner))
            .unwrap();
        assert_eq!(
            finding.evidence["board_health_finding"], *health_finding,
            "persisted evidence must preserve the complete board-health child snapshot"
        );
        let snapshot = finding.resolver_snapshot.as_ref().unwrap();
        assert_eq!(snapshot["resolver"], "resolve_closed_parent_open_children");
        assert_eq!(
            snapshot["inputs"],
            json!({ "board_health_finding": health_finding }),
            "dry-run resolver inputs must retain the complete health finding"
        );
        assert_eq!(
            snapshot["outputs"],
            json!({
                "selected_disposition": health_finding["recommended_disposition"],
                "would_mutate": matches!(
                    health_finding["recommended_action"].as_str(),
                    Some("close" | "park")
                ),
            }),
            "dry-run resolver output must expose the selected matrix row and mutation decision"
        );
        actual.insert(
            owner.to_owned(),
            (
                finding.evidence["selected_disposition"]["action"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                finding.evidence["selected_disposition"]["guard"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            ),
        );
    }
    let expected: std::collections::BTreeMap<_, _> = expected
        .into_iter()
        .map(|(id, (action, guard))| (id.to_owned(), (action.to_owned(), guard.to_owned())))
        .collect();
    assert_eq!(actual, expected);
    let mut tasks_after = Vec::new();
    let mut activity_after = Vec::new();
    for id in &ids {
        tasks_after.push(serde_json::to_value(tasks.get(id).await.unwrap().unwrap()).unwrap());
        activity_after.push(serde_json::to_value(tasks.list_activity(id).await.unwrap()).unwrap());
    }
    assert_eq!(tasks_before, tasks_after);
    assert_eq!(activity_before, activity_after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_db_repair_applies_safe_disposition() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, ProposalCreateInput, ProposalRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());
    let proposals = ProposalRepository::new(harness.db().clone(), common::test_events());

    // Ready orphan closes; in-flight parks retaining session; PR-review parks retaining PR.
    let ready_epic = common::create_test_epic(harness.db(), &project.id).await;
    let ready = common::create_test_task(harness.db(), &project.id, &ready_epic.id).await;
    epics
        .set_status_raw(&ready_epic.id, "closed")
        .await
        .unwrap();

    let flight_epic = common::create_test_epic(harness.db(), &project.id).await;
    let flight = common::create_test_task(harness.db(), &project.id, &flight_epic.id).await;
    tasks.set_status(&flight.id, "in_progress").await.unwrap();
    let flight_session = common::create_test_session(harness.db(), &project.id, &flight.id).await;
    epics
        .set_status_raw(&flight_epic.id, "closed")
        .await
        .unwrap();

    let pr_epic = common::create_test_epic(harness.db(), &project.id).await;
    let pr = common::create_test_task(harness.db(), &project.id, &pr_epic.id).await;
    tasks.set_status(&pr.id, "pr_review").await.unwrap();
    tasks
        .set_pr_url(&pr.id, "https://github.com/djinnos/djinn/pull/999999")
        .await
        .unwrap();
    epics.set_status_raw(&pr_epic.id, "closed").await.unwrap();

    // This row is actionable in the snapshot, then gains another open proposal
    // parent before repair. That exercises the lock-time other-parent guard.
    let guard_epic = common::create_test_epic(harness.db(), &project.id).await;
    let guard = common::create_test_task(harness.db(), &project.id, &guard_epic.id).await;
    epics
        .set_status_raw(&guard_epic.id, "closed")
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;

    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    assert_eq!(run["ok"], true);
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 4);
    let repo = DoctorFindingRepository::new(harness.db().clone());

    let mut by_task: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for result in findings {
        let fid = result["finding_id"].as_str().unwrap().to_owned();
        let finding = repo.get(&fid).await.unwrap().unwrap();
        let owner = finding.entity_ids["task_id"].as_str().unwrap().to_owned();
        by_task.insert(owner, fid);
    }

    // Add the open parent only after doctor_run persists the actionable finding.
    // Linking it to `guard_epic` changes the task's live disposition without
    // introducing an unrelated closed-parent/open-child finding.
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

    // Apply repair to each persisted finding. `result` is the additive MCP
    // contract: retain every response so this test locks the applied and
    // guarded outcome tags, not merely the top-level success envelope.
    let mut results = std::collections::BTreeMap::new();
    for (task_id, fid) in &by_task {
        let fix = harness
            .call_tool(
                "doctor_fix",
                json!({
                    "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                    "finding_id": fid,
                }),
            )
            .await
            .unwrap();
        assert_eq!(fix["ok"], true);
        assert_eq!(fix["check_name"], CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME);
        assert_eq!(fix["finding_id"], *fid);
        assert_eq!(fix["error"], serde_json::Value::Null);
        results.insert(task_id.clone(), fix["result"].clone());
    }

    assert_eq!(
        results[&ready.id],
        json!({ "outcome": "closed", "task_id": ready.id, "from_status": "open" })
    );
    assert_eq!(
        results[&flight.id],
        json!({
            "outcome": "parked",
            "task_id": flight.id,
            "from_status": "in_progress",
            "reason": "historical_parent_closed_in_flight",
        })
    );
    assert_eq!(
        results[&pr.id],
        json!({
            "outcome": "parked",
            "task_id": pr.id,
            "from_status": "pr_review",
            "reason": "historical_parent_closed_pr_active",
        })
    );
    assert_eq!(
        results[&guard.id],
        json!({ "outcome": "skipped_other_open_parent", "task_id": guard.id })
    );

    // Assert outcomes.
    let ready_row = tasks.get(&ready.id).await.unwrap().unwrap();
    assert_eq!(ready_row.status, "closed");
    assert_eq!(ready_row.close_reason.as_deref(), Some("parent_closed"));

    let flight_row = tasks.get(&flight.id).await.unwrap().unwrap();
    assert_eq!(flight_row.status, "needs_lead_intervention");
    let flight_activity = tasks.list_activity(&flight.id).await.unwrap();
    let flight_repair = flight_activity
        .iter()
        .find(|e| e.event_type == "doctor_fix_repair")
        .expect("flight task must have doctor_fix_repair activity");
    let flight_payload: serde_json::Value = serde_json::from_str(&flight_repair.payload).unwrap();
    assert_eq!(
        flight_payload["park_reason"].as_str(),
        Some("historical_parent_closed_in_flight")
    );
    assert_eq!(
        flight_payload["preserved_session_id"].as_str(),
        Some(flight_session.id.as_str())
    );

    let pr_row = tasks.get(&pr.id).await.unwrap().unwrap();
    assert_eq!(pr_row.status, "needs_lead_intervention");
    assert_eq!(
        pr_row.pr_url.as_deref(),
        Some("https://github.com/djinnos/djinn/pull/999999")
    );
    let pr_activity = tasks.list_activity(&pr.id).await.unwrap();
    let pr_repair = pr_activity
        .iter()
        .find(|e| e.event_type == "doctor_fix_repair")
        .expect("pr task must have doctor_fix_repair activity");
    let pr_payload: serde_json::Value = serde_json::from_str(&pr_repair.payload).unwrap();
    assert_eq!(
        pr_payload["park_reason"].as_str(),
        Some("historical_parent_closed_pr_active")
    );
    assert_eq!(
        pr_payload["preserved_pr_url"].as_str(),
        Some("https://github.com/djinnos/djinn/pull/999999")
    );

    let guard_row = tasks.get(&guard.id).await.unwrap().unwrap();
    assert_eq!(guard_row.status, "open");
    let guard_activity = tasks.list_activity(&guard.id).await.unwrap();
    let guard_repair = guard_activity
        .iter()
        .find(|e| e.event_type == "doctor_fix_repair");
    assert!(
        guard_repair.is_none(),
        "other-open-parent skip must not emit doctor_fix_repair activity"
    );

    // Every mutation emits both the normal lifecycle activity and an audit
    // activity whose provenance and original terminal parent are exact.
    for (id, from_status, to_status, reason, session, pr_url, parent_id) in [
        (
            &ready.id,
            "open",
            "closed",
            "parent_closed",
            None,
            None,
            &ready_epic.id,
        ),
        (
            &flight.id,
            "in_progress",
            "needs_lead_intervention",
            "historical_parent_closed_in_flight",
            Some(flight_session.id.as_str()),
            None,
            &flight_epic.id,
        ),
        (
            &pr.id,
            "pr_review",
            "needs_lead_intervention",
            "historical_parent_closed_pr_active",
            None,
            Some("https://github.com/djinnos/djinn/pull/999999"),
            &pr_epic.id,
        ),
    ] {
        let activity = tasks.list_activity(id).await.unwrap();
        let repair = activity
            .iter()
            .find(|event| event.event_type == "doctor_fix_repair")
            .unwrap_or_else(|| panic!("task {id} should have doctor_fix_repair activity"));
        let repair: serde_json::Value = serde_json::from_str(&repair.payload).unwrap();
        assert_eq!(repair["source"], "doctor_fix");
        assert_eq!(repair["check"], "closed_parent_open_children");
        assert_eq!(repair["original_parent_ids"], json!([parent_id]));
        assert_eq!(repair["from_status"], from_status);
        assert_eq!(repair["to_status"], to_status);
        assert_eq!(repair["reason"], reason);
        assert_eq!(repair["preserved_session_id"].as_str(), session);
        assert_eq!(repair["preserved_pr_url"].as_str(), pr_url);

        let status = activity
            .iter()
            .find(|event| event.event_type == "status_changed")
            .unwrap_or_else(|| panic!("task {id} should have status_changed activity"));
        let status: serde_json::Value = serde_json::from_str(&status.payload).unwrap();
        assert_eq!(
            status,
            json!({
                "from_status": from_status,
                "to_status": to_status,
                "reason": reason,
            })
        );
    }

    // Repeating the exact same persisted finding is safe and reports the
    // guard that prevented a second mutation.
    let repeated_ready = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                "finding_id": by_task[&ready.id],
            }),
        )
        .await
        .unwrap();
    assert_eq!(repeated_ready["ok"], true);
    assert_eq!(
        repeated_ready["result"],
        json!({ "outcome": "skipped_already_closed", "task_id": ready.id })
    );

    // Idempotency: a second repair run reports no actionable findings.
    let run2 = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings2 = run2["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings2.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_repair_skips_external_open_dependent() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());

    let orphan_epic = common::create_test_epic(harness.db(), &project.id).await;
    let orphan = common::create_test_task(harness.db(), &project.id, &orphan_epic.id).await;
    epics
        .set_status_raw(&orphan_epic.id, "closed")
        .await
        .unwrap();

    // Open dependent in a different epic; orphan blocks it.
    let other_epic = common::create_test_epic(harness.db(), &project.id).await;
    let dependent = common::create_test_task(harness.db(), &project.id, &other_epic.id).await;
    tasks.add_blocker(&dependent.id, &orphan.id).await.unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = std::sync::Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["recommended_action"], "retain");
    assert_eq!(findings[0]["recommended_reason"], "external_open_dependent");

    let fid = findings[0]["finding_id"].as_str().unwrap().to_owned();
    let fix = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                "finding_id": fid,
            }),
        )
        .await
        .unwrap();
    assert_eq!(fix["ok"], true);
    assert_eq!(fix["check_name"], CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME);
    assert_eq!(fix["finding_id"], fid);
    assert_eq!(
        fix["result"],
        json!({ "outcome": "skipped_retain", "task_id": orphan.id })
    );

    let orphan_row = tasks.get(&orphan.id).await.unwrap().unwrap();
    assert_eq!(orphan_row.status, "open");
    let dependent_row = tasks.get(&dependent.id).await.unwrap().unwrap();
    assert_eq!(dependent_row.status, "open");

    let activity = tasks.list_activity(&orphan.id).await.unwrap();
    assert!(
        !activity.iter().any(|e| e.event_type == "doctor_fix_repair"),
        "external-dependent orphan must not be repaired"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_repair_cascades_internal_blocker() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());

    let parent_epic = common::create_test_epic(harness.db(), &project.id).await;
    let blocker = common::create_test_task(harness.db(), &project.id, &parent_epic.id).await;
    let dependent = common::create_test_task(harness.db(), &project.id, &parent_epic.id).await;
    tasks.add_blocker(&dependent.id, &blocker.id).await.unwrap();
    epics
        .set_status_raw(&parent_epic.id, "closed")
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = std::sync::Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    for finding in findings {
        assert_eq!(finding["recommended_action"], "close");
        assert_eq!(finding["recommended_reason"], "parent_closed");
    }

    let repo = DoctorFindingRepository::new(harness.db().clone());
    for result in findings {
        let fid = result["finding_id"].as_str().unwrap().to_owned();
        let finding = repo.get(&fid).await.unwrap().unwrap();
        let fix = harness
            .call_tool(
                "doctor_fix",
                json!({
                    "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                    "finding_id": fid,
                }),
            )
            .await
            .unwrap();
        assert_eq!(fix["ok"], true);

        let task_id = finding.entity_ids["task_id"].as_str().unwrap();
        let row = tasks.get(task_id).await.unwrap().unwrap();
        assert_eq!(row.status, "closed");
        assert_eq!(row.close_reason.as_deref(), Some("parent_closed"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_parent_open_children_repair_skips_stale_snapshot() {
    use djinn_agent::doctor::{
        CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME, TaskRepositoryClosedParentOpenChildrenSource,
        register_closed_parent_open_children_check_with_repair,
    };
    use djinn_core::events::DjinnEventEnvelope;
    use djinn_db::{EpicRepository, TaskRepository};
    let harness = doctor_test_harness().await;
    let project = common::create_test_project(harness.db()).await;
    let epics = EpicRepository::new(harness.db().clone(), common::test_events());
    let tasks = TaskRepository::new(harness.db().clone(), common::test_events());

    let ready_epic = common::create_test_epic(harness.db(), &project.id).await;
    let ready = common::create_test_task(harness.db(), &project.id, &ready_epic.id).await;
    epics
        .set_status_raw(&ready_epic.id, "closed")
        .await
        .unwrap();

    let (tx, _) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let source = std::sync::Arc::new(TaskRepositoryClosedParentOpenChildrenSource::new(
        harness.db().clone(),
        tx,
    ));
    source.refresh().await;
    register_closed_parent_open_children_check_with_repair(
        registry(),
        source.clone(),
        source.clone(),
    );

    let run = harness
        .call_tool(
            "doctor_run",
            json!({"check_names":[CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME]}),
        )
        .await
        .unwrap();
    let findings = run["results"][0]["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    let fid = findings[0]["finding_id"].as_str().unwrap().to_owned();

    // Simulate a non-terminal status transition before repair. The finding's
    // `open` snapshot is stale and must not be allowed to park or close it.
    tasks.set_status(&ready.id, "in_progress").await.unwrap();

    let fix = harness
        .call_tool(
            "doctor_fix",
            json!({
                "check_name": CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME,
                "finding_id": fid,
            }),
        )
        .await
        .unwrap();
    assert_eq!(fix["ok"], true);
    assert_eq!(fix["check_name"], CLOSED_PARENT_OPEN_CHILDREN_CHECK_NAME);
    assert_eq!(fix["finding_id"], fid);
    assert_eq!(
        fix["result"],
        json!({
            "outcome": "skipped_status_drift",
            "task_id": ready.id,
            "snapshot_status": "open",
            "current_status": "in_progress",
        })
    );

    let row = tasks.get(&ready.id).await.unwrap().unwrap();
    assert_eq!(row.status, "in_progress");
    let activity = tasks.list_activity(&ready.id).await.unwrap();
    assert!(
        !activity.iter().any(|e| e.event_type == "doctor_fix_repair"),
        "stale snapshot must not be repaired"
    );
}
