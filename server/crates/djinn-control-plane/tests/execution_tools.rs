// Test-only: Instant::now is used for deadlined poll loops in these
// integration tests.
#![allow(clippy::disallowed_methods)]
//! Contract tests for `execution_*` MCP tools.
//!
//! The nonexistent-task test intentionally uses strict stubs so it pins the MCP
//! error envelope.  The real-pool smoke test below exercises the same
//! `execution_kill_task` tool dispatch path with a real `SlotPoolHandle`, while
//! keeping runtime/Kubernetes effects behind a recording test bridge.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use djinn_agent::actors::slot::{
    ModelSlotConfig, SlotFactory, SlotHandle, SlotPoolConfig, SlotPoolHandle,
};
use djinn_agent::context::{ActivityTracker, AgentContext, ReconciliationSweepConfig};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_control_plane::bridge::{
    ModelPoolStatus, PoolStatus, ReconcileTerminateExecution, ReconcileTerminateKind,
    ReconcileTerminateObservations, ReconcileTerminateSnapshot, RunningTaskInfo, RuntimeOps,
    SlotPoolOps,
};
use djinn_control_plane::state::McpState;
use djinn_control_plane::test_support::{
    McpTestHarness, StubCoordinator, StubGit, StubLsp, StubNoteEmbedding, StubNoteVectorStore,
    StubRepoGraph,
};
use djinn_core::events::EventBus;
use djinn_core::models::{DjinnSettings, SessionStatus};
use djinn_db::test_support::{
    liveness_evidence_for_task_for_test, reject_task_liveness_evidence_for_test,
    set_task_short_id_for_test,
};
use djinn_db::{
    CreateSessionParams, CreateTaskRunParams, Database, EffectiveCreatorProvenance,
    EpicCreateInput, EpicRepository, LivenessRepository, ProjectRepository, SessionRepository,
    TaskRepository, TaskRunRepository, UserRepository,
};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use serde_json::json;
use tokio::sync::{Mutex as TokioMutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn execution_kill_task_with_nonexistent_task_returns_error_shape() {
    let harness = McpTestHarness::new().await;

    let response = harness
        .call_tool(
            "execution_kill_task",
            json!({"task_id":"nonexistent-task-id"}),
        )
        .await
        .expect("execution_kill_task should dispatch");

    assert_eq!(response["ok"], false);
    assert_eq!(response["kind"], "task_not_found");
    assert_eq!(response["task_id"], serde_json::Value::Null);
    assert!(response.get("error").and_then(|v| v.as_str()).is_some());
}

fn controlled_completion_slot_factory(
    race: CompletionRaceControl,
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
) -> SlotFactory {
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let race = race.clone();
        let signal_tx = signal_tx.clone();
        let runner: djinn_agent::actors::slot::TestLifecycleRunner = Arc::new(
            move |task_id,
                  _execution_generation,
                  _project_path,
                  _model_id,
                  app_state,
                  kill,
                  _pause,
                  _resume_lifecycle_metadata| {
                let race = race.clone();
                let signal_tx = signal_tx.clone();
                Box::pin(async move {
                    let _ = signal_tx.send(RunnerSignal::Started(task_id.clone()));
                    let _ = signal_tx.send(RunnerSignal::WaitingToComplete(task_id.clone()));

                    tokio::select! {
                        _ = race.allow_natural_settlement.notified() => {
                            let session_repo = SessionRepository::new(
                                app_state.db.clone(),
                                app_state.event_bus.clone(),
                            );
                            for session in session_repo
                                .list_for_task(&task_id)
                                .await?
                                .into_iter()
                                .filter(|session| session.status == SessionStatus::Running.as_str())
                            {
                                session_repo
                                    .update(
                                        &session.id,
                                        SessionStatus::Completed,
                                        0,
                                        0,
                                        0,
                                        0,
                                        None,
                                    )
                                    .await?;
                            }
                            let _ = signal_tx.send(RunnerSignal::NaturallySettled(task_id.clone()));
                            kill.cancelled().await;
                            let _ = signal_tx.send(RunnerSignal::Killed(task_id));
                        }
                        _ = kill.cancelled() => {
                            let _ = signal_tx.send(RunnerSignal::Killed(task_id));
                        }
                    }
                    Ok(())
                })
            },
        );

        SlotHandle::spawn_with_test_runner(slot_id, model_id, event_tx, app_state, cancel, runner)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_settles_live_run_through_control_plane_tool_route() {
    let harness = RealPoolKillHarness::new().await;
    let seeded = harness
        .seed_running_session_with_task_run("kill-smoke-run")
        .await;

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_runner_started(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    assert!(
        harness.pool_has_session(&seeded.task_id).await,
        "dispatched task must be visible through the real pool before kill"
    );
    assert_eq!(
        harness.running_task_ids().await,
        vec![seeded.task_id.clone()],
        "real-pool bridge status should expose the dispatched task before kill"
    );
    harness.assert_pool_capacity(1, 0).await;
    assert_eq!(harness.running_count_for_cap().await, 1);

    let response = harness
        .call_kill_tool(&seeded.task_id)
        .await
        .expect("execution_kill_task should dispatch");

    assert_eq!(response["ok"], true);
    assert_eq!(response["task_id"], seeded.task_id);
    assert_eq!(response["error"], serde_json::Value::Null);
    harness.wait_for_runner_killed(&seeded.task_id).await;
    assert!(
        !harness.pool_has_session(&seeded.task_id).await,
        "tool must confirm the real pool mapping was reclaimed"
    );
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![seeded.task_run_id.clone()],
        "real pool termination should route task-run teardown through RuntimeOps"
    );

    let session = harness.session(&seeded.session_id).await;
    assert_eq!(session.status, SessionStatus::Interrupted.as_str());
    assert!(
        session.ended_at.is_some(),
        "terminated session is stamped ended_at"
    );
    assert!(
        harness.active_sessions().await.is_empty(),
        "no active DB sessions should remain after terminate_session"
    );
    assert_eq!(
        harness.running_count_for_cap().await,
        0,
        "settled session must not count against per-user/model capacity"
    );
    harness.wait_for_pool_capacity(0, 1).await;
    assert!(
        harness.running_task_ids().await.is_empty(),
        "pool status should not report running tasks after kill settlement"
    );
    harness
        .assert_single_terminal_session(&seeded.task_id, SessionStatus::Interrupted)
        .await;

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    assert!(
        harness.pool_has_session(&seeded.task_id).await,
        "terminated task should be redispatchable through the real pool"
    );
    harness.assert_pool_capacity(1, 0).await;
    harness.shutdown();
}

/// The MCP response is the actor's canonical reconciliation snapshot, not a
/// latest-session projection. This fixture leaves two durable sessions under
/// one live pool mapping and requires the one audit row to preserve the full
/// ordered snapshot verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_mapped_duplicate_sessions_preserves_response_and_audit_evidence() {
    let harness = RealPoolKillHarness::new().await;
    let first = harness
        .seed_running_session_with_task_run("mapped-duplicate-first-run")
        .await;
    let second = harness
        .seed_duplicate_running_session(&first, "mapped-duplicate-second-run")
        .await;
    let sessions = SessionRepository::new(
        harness.app_state.db.clone(),
        harness.app_state.event_bus.clone(),
    )
    .list_non_terminal_for_task(&first.task_id)
    .await
    .expect("complete deterministic duplicate capture");
    let initial_ids: Vec<_> = sessions.iter().map(|session| session.id.clone()).collect();
    let initial_runs: Vec<_> = sessions
        .iter()
        .map(|session| {
            session
                .task_run_id
                .clone()
                .expect("duplicate fixture sessions have non-null task-run IDs")
        })
        .collect();
    assert_eq!(initial_ids.len(), 2);
    assert!(initial_ids.contains(&first.session_id));
    assert!(initial_ids.contains(&second.session_id));
    assert_ne!(initial_runs[0], initial_runs[1]);
    let expected_executions = json!(
        sessions
            .iter()
            .map(|session| json!({
                "session_id": session.id,
                "task_run_id": session.task_run_id,
                "teardown_owner": true,
                "teardown_attempted": true,
                "teardown_error": null,
                "settlement_attempted": true,
                "settlement_error": null,
            }))
            .collect::<Vec<_>>()
    );

    harness.dispatch(&first.task_id).await;
    harness.wait_for_runner_started(&first.task_id).await;
    harness.wait_for_pool_session(&first.task_id).await;
    assert!(harness.pool_has_session(&first.task_id).await);

    let response = harness
        .call_kill_tool(&first.task_id)
        .await
        .expect("mapped duplicate execution_kill_task should dispatch");

    assert_eq!(response["ok"], true);
    assert_eq!(response["kind"], "desync_reconciled");
    assert_eq!(response["task_id"], first.task_id);
    assert_eq!(response["error"], serde_json::Value::Null);
    assert_eq!(response["executions"], expected_executions);
    assert_eq!(
        response["observations"]["initial_non_terminal_ids"],
        json!(initial_ids)
    );
    assert_eq!(response["observations"]["initial_mapping_slot_id"], 0);
    assert_eq!(
        response["observations"]["final_non_terminal_ids"],
        json!([])
    );
    assert_eq!(
        response["observations"]["final_mapping_slot_id"],
        serde_json::Value::Null
    );
    assert_eq!(response["observations"]["final_pending_teardown"], false);
    assert_eq!(response["observations"]["completion_source"], "immediate");

    harness.wait_for_runner_killed(&first.task_id).await;
    assert!(!harness.pool_has_session(&first.task_id).await);
    assert!(harness.running_task_ids().await.is_empty());
    assert!(harness.active_sessions().await.is_empty());
    assert_eq!(harness.runtime_teardown_calls(), initial_runs);
    for session_id in [&first.session_id, &second.session_id] {
        let session = harness.session(session_id).await;
        assert_eq!(session.status, SessionStatus::Interrupted.as_str());
        assert!(
            session.ended_at.is_some(),
            "captured session {session_id} is terminal"
        );
    }

    let evidence = liveness_evidence_for_task_for_test(&harness.app_state.db, &first.task_id).await;
    assert_eq!(
        evidence.len(),
        1,
        "one outward invocation writes one audit row"
    );
    let audit = &evidence[0].1;
    assert_eq!(audit["task_id"], response["task_id"]);
    assert_eq!(audit["kind"], response["kind"]);
    assert_eq!(audit["executions"], response["executions"]);
    assert_eq!(audit["observations"], response["observations"]);
    harness.shutdown();
}

/// Shared task-run rows still settle independently, but exactly one captured
/// execution owns the runtime teardown for that run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_mapped_duplicate_sessions_deduplicates_shared_run_teardown() {
    let harness = RealPoolKillHarness::new().await;
    let first = harness
        .seed_running_session_with_task_run("mapped-duplicate-shared-run")
        .await;
    let second = harness
        .seed_duplicate_session_for_existing_task_run(&first)
        .await;

    harness.dispatch(&first.task_id).await;
    harness.wait_for_runner_started(&first.task_id).await;
    harness.wait_for_pool_session(&first.task_id).await;
    let response = harness
        .call_kill_tool(&first.task_id)
        .await
        .expect("mapped shared-run duplicate execution_kill_task should dispatch");

    assert_eq!(response["ok"], true);
    assert_eq!(response["kind"], "desync_reconciled");
    assert_eq!(response["observations"]["initial_mapping_slot_id"], 0);
    assert_eq!(
        response["observations"]["final_non_terminal_ids"],
        json!([])
    );
    assert_eq!(
        response["observations"]["final_mapping_slot_id"],
        serde_json::Value::Null
    );
    assert_eq!(response["observations"]["final_pending_teardown"], false);
    let executions = response["executions"]
        .as_array()
        .expect("shared-run response executions");
    assert_eq!(executions.len(), 2);
    assert!(executions.iter().all(|execution| {
        execution["task_run_id"] == first.task_run_id
            && execution["settlement_attempted"] == true
            && execution["settlement_error"].is_null()
    }));
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution["teardown_owner"] == true)
            .count(),
        1,
        "one shared-run execution owns teardown"
    );
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution["teardown_attempted"] == true)
            .count(),
        1,
        "shared run is torn down once"
    );

    harness.wait_for_runner_killed(&first.task_id).await;
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![first.task_run_id.clone()]
    );
    assert!(!harness.pool_has_session(&first.task_id).await);
    for session_id in [&first.session_id, &second.session_id] {
        let session = harness.session(session_id).await;
        assert_eq!(session.status, SessionStatus::Interrupted.as_str());
        assert!(session.ended_at.is_some());
    }
    let evidence = liveness_evidence_for_task_for_test(&harness.app_state.db, &first.task_id).await;
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].1["executions"], response["executions"]);
    assert_eq!(evidence[0].1["observations"], response["observations"]);
    harness.shutdown();
}

/// The historical `yf6r` regression passed the raw short id to the pool, so
/// the durable row and live mapping were observed under different task ids.
/// This drives the real MCP route with that exact id and proves resolution
/// happens before actor-owned reconciliation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_yf6r_short_id_and_uuid_share_authoritative_task() {
    let harness = RealPoolKillHarness::new().await;
    let seeded = harness
        .seed_running_session_with_task_run("yf6r-authoritative-run")
        .await;
    set_task_short_id_for_test(&harness.app_state.db, &seeded.task_id, "yf6r").await;

    let tasks = TaskRepository::new(
        harness.app_state.db.clone(),
        harness.app_state.event_bus.clone(),
    );
    assert_eq!(
        tasks
            .resolve("yf6r")
            .await
            .expect("short-id resolution")
            .expect("task")
            .id,
        seeded.task_id,
        "yf6r must resolve to the same canonical UUID as the UUID form"
    );
    assert_eq!(
        tasks
            .resolve(&seeded.task_id)
            .await
            .expect("uuid resolution")
            .expect("task")
            .id,
        seeded.task_id
    );

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_runner_started(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    let short_response = harness
        .call_kill_tool("yf6r")
        .await
        .expect("short-id kill should dispatch");
    assert_eq!(short_response["ok"], true);
    assert_eq!(short_response["kind"], "terminated");
    assert_eq!(short_response["task_id"], seeded.task_id);
    assert_eq!(short_response["observations"]["initial_mapping_slot_id"], 0);
    assert_eq!(
        short_response["observations"]["initial_non_terminal_ids"],
        json!([seeded.session_id.clone()]),
        "the short-id request must retain the complete durable capture"
    );
    assert_eq!(
        short_response["observations"]["final_non_terminal_ids"],
        json!([])
    );
    assert_eq!(
        short_response["observations"]["final_mapping_slot_id"],
        serde_json::Value::Null
    );
    assert_eq!(
        short_response["observations"]["completion_source"],
        "immediate"
    );
    assert_eq!(
        short_response["executions"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        short_response["executions"][0]["session_id"],
        seeded.session_id
    );
    assert_eq!(
        short_response["executions"][0]["task_run_id"],
        seeded.task_run_id
    );
    assert_eq!(short_response["executions"][0]["teardown_attempted"], true);
    assert_eq!(
        short_response["executions"][0]["settlement_attempted"],
        true
    );

    harness.wait_for_runner_killed(&seeded.task_id).await;
    harness
        .assert_single_terminal_session(&seeded.task_id, SessionStatus::Interrupted)
        .await;
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![seeded.task_run_id.clone()]
    );
    assert!(!harness.pool_has_session(&seeded.task_id).await);
    let audit_json = liveness_evidence_for_task_for_test(&harness.app_state.db, &seeded.task_id)
        .await
        .into_iter()
        .next()
        .expect("short-id call audit JSON")
        .1;
    assert_eq!(audit_json["task_id"], seeded.task_id);
    assert_eq!(audit_json["kind"], "terminated");
    assert_eq!(
        audit_json["observations"]["initial_non_terminal_ids"],
        json!([seeded.session_id.clone()]),
        "audit retains the verbatim authoritative snapshot"
    );

    // A UUID invocation after the short-id call must still be attributed to
    // the canonical task and contributes one, separate outward-call audit row.
    let uuid_response = harness
        .call_kill_tool(&seeded.task_id)
        .await
        .expect("uuid kill");
    assert_eq!(uuid_response["task_id"], seeded.task_id);
    assert_eq!(uuid_response["kind"], "genuinely_absent");
    assert_eq!(uuid_response["ok"], true);
    assert_eq!(
        LivenessRepository::new(harness.app_state.db.clone())
            .count_evidence_for_task(&seeded.task_id)
            .await
            .expect("audit evidence count"),
        2,
        "each resolved short-id/UUID invocation writes exactly one audit row"
    );
    harness.shutdown();
}

/// Audit persistence is deliberately failed at the real repository boundary,
/// after the real pool has reconciled and settled the captured execution. The
/// outward failure must retain the actor's complete authoritative snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_audit_failure_retains_real_pool_snapshot() {
    let harness = RealPoolKillHarness::new().await;
    let seeded = harness
        .seed_running_session_with_task_run("audit-failure-run")
        .await;
    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_runner_started(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;

    // Fail the append-only evidence insert without replacing SlotPoolHandle or
    // its reconciliation bridge with a mock.
    reject_task_liveness_evidence_for_test(&harness.app_state.db).await;

    let response = harness
        .call_kill_tool(&seeded.task_id)
        .await
        .expect("audit failure remains a tool response");
    assert_eq!(response["ok"], false);
    assert_eq!(response["kind"], "audit_failed");
    assert_eq!(response["underlying_kind"], "terminated");
    assert_eq!(response["task_id"], seeded.task_id);
    assert_eq!(
        response["executions"],
        json!([{
            "session_id": seeded.session_id,
            "task_run_id": seeded.task_run_id,
            "teardown_owner": true,
            "teardown_attempted": true,
            "teardown_error": null,
            "settlement_attempted": true,
            "settlement_error": null
        }]),
        "audit_failed must retain the ordered actor execution evidence"
    );
    let observations = response["observations"]
        .as_object()
        .expect("audit failure retains observations");
    assert_eq!(
        observations.len(),
        13,
        "all observation fields are retained"
    );
    assert_eq!(
        observations["initial_non_terminal_ids"],
        json!([seeded.session_id])
    );
    assert_eq!(observations["initial_mapping_slot_id"], 0);
    assert_eq!(observations["initial_pending_teardown"], false);
    assert_eq!(observations["initial_compacting"], false);
    assert!(observations["fenced_generation"].as_i64().is_some());
    assert_eq!(
        observations["initial_capture_error"],
        serde_json::Value::Null
    );
    assert_eq!(observations["final_non_terminal_ids"], json!([]));
    assert_eq!(
        observations["final_mapping_slot_id"],
        serde_json::Value::Null
    );
    assert_eq!(observations["final_pending_teardown"], false);
    assert_eq!(observations["final_reread_error"], serde_json::Value::Null);
    assert_eq!(observations["pool_cleanup_error"], serde_json::Value::Null);
    assert_eq!(observations["completion_source"], "immediate");
    assert_eq!(observations["underlying_kind"], serde_json::Value::Null);
    assert!(
        response["error"]
            .as_str()
            .is_some_and(|error| error.contains("reject_execution_kill_evidence"))
    );
    assert_eq!(
        LivenessRepository::new(harness.app_state.db.clone())
            .count_evidence_for_task(&seeded.task_id)
            .await
            .expect("failed audit leaves no row"),
        0
    );
    harness.wait_for_runner_killed(&seeded.task_id).await;
    assert_settled_after_kill(&harness, &seeded).await;
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![seeded.task_run_id.clone()],
        "the underlying real-pool reconciliation completed before audit failed"
    );
    harness.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_racing_natural_completion_settles_once_and_releases_capacity() {
    let race = CompletionRaceControl::default();
    let harness = RealPoolKillHarness::new_with_slot_factory({
        let race = race.clone();
        move |signal_tx| controlled_completion_slot_factory(race, signal_tx)
    })
    .await;
    let seeded = harness
        .seed_running_session_with_task_run("kill-completion-race-run")
        .await;

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_runner_started(&seeded.task_id).await;
    harness
        .wait_for_runner_waiting_to_complete(&seeded.task_id)
        .await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    harness.assert_pool_capacity(1, 0).await;
    assert_eq!(harness.running_count_for_cap().await, 1);

    // Deterministic interleaving: let the fake lifecycle perform the same DB
    // terminal write that a naturally finishing worker would do, but keep it
    // parked before returning to the slot actor.  The control-plane kill then
    // arrives while natural completion is in progress (the session is ended, the
    // pool still owns the task->slot mapping, and no Free/Killed event has been
    // emitted yet).  This avoids sleeps/Kubernetes timing while pinning the
    // historical duplicate-settlement/free-list race at the integration seam.
    race.allow_natural_settlement();
    harness
        .wait_for_runner_naturally_settled(&seeded.task_id)
        .await;

    let response = harness
        .call_kill_tool(&seeded.task_id)
        .await
        .expect("execution_kill_task should dispatch during completion race");

    assert_eq!(response["ok"], true);
    assert_eq!(response["task_id"], seeded.task_id);
    assert_eq!(response["error"], serde_json::Value::Null);
    harness.wait_for_runner_killed(&seeded.task_id).await;
    harness.wait_for_pool_capacity(0, 1).await;

    assert!(
        !harness.pool_has_session(&seeded.task_id).await,
        "completion/kill race must leave no active pool session"
    );
    assert!(
        harness.running_task_ids().await.is_empty(),
        "pool status should not report the raced task as running"
    );
    assert!(
        harness.active_sessions().await.is_empty(),
        "naturally settled session should no longer be active after raced kill"
    );
    harness
        .assert_single_terminal_session(&seeded.task_id, SessionStatus::Completed)
        .await;
    assert_eq!(
        harness.running_count_for_cap().await,
        0,
        "raced settlement must not leak per-user/model capacity"
    );

    let session = harness.session(&seeded.session_id).await;
    assert_eq!(
        session.status,
        SessionStatus::Completed.as_str(),
        "kill must not overwrite the natural terminal settlement"
    );
    assert!(
        session.ended_at.is_some(),
        "raced terminal session row must be internally consistent"
    );
    assert!(
        harness.runtime_teardown_calls().is_empty(),
        "natural completion already ended the session before kill, so runtime teardown must not be duplicated"
    );

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    harness.assert_pool_capacity(1, 0).await;
    assert_eq!(
        harness.running_task_ids().await,
        vec![seeded.task_id.clone()],
        "subsequent dispatch proves no stale free-list/capacity wedge remained"
    );
    harness.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_kill_task_double_kill_is_harmless_and_leaves_capacity_available() {
    let harness = RealPoolKillHarness::new().await;
    let seeded = harness
        .seed_running_session_with_task_run("double-kill-run")
        .await;

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_runner_started(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    assert!(
        harness.pool_has_session(&seeded.task_id).await,
        "precondition: dispatched task must have an active pool session"
    );
    harness.assert_pool_capacity(1, 0).await;
    assert_eq!(harness.running_count_for_cap().await, 1);

    let first_response = harness
        .call_kill_tool(&seeded.task_id)
        .await
        .expect("first execution_kill_task should dispatch");

    assert_eq!(first_response["ok"], true);
    assert_eq!(first_response["task_id"], seeded.task_id);
    assert_eq!(first_response["error"], serde_json::Value::Null);
    harness.wait_for_runner_killed(&seeded.task_id).await;
    harness.wait_for_pool_capacity(0, 1).await;

    assert_settled_after_kill(&harness, &seeded).await;
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![seeded.task_run_id.clone()],
        "first kill should perform exactly one task-run teardown"
    );

    let second_response = harness
        .call_kill_tool(&seeded.task_id)
        .await
        .expect("second execution_kill_task should still return a tool response");

    assert_truthful_harmless_second_kill_response(&second_response, &seeded.task_id);
    assert_eq!(
        LivenessRepository::new(harness.app_state.db.clone())
            .count_evidence_for_task(&seeded.task_id)
            .await
            .expect("evidence count"),
        2,
        "each resolved outward kill invocation writes exactly one audit row"
    );
    assert_settled_after_kill(&harness, &seeded).await;
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![seeded.task_run_id.clone()],
        "repeated kill must not duplicate task-run teardown"
    );
    harness.wait_for_pool_capacity(0, 1).await;

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    harness.assert_pool_capacity(1, 0).await;
    assert_eq!(
        harness.running_task_ids().await,
        vec![seeded.task_id.clone()],
        "subsequent dispatch proves repeated kill did not poison free-list/capacity state"
    );
    assert_eq!(
        harness.running_count_for_cap().await,
        0,
        "redispatching the same DB session fixture must not resurrect an active DB session row"
    );
    harness.shutdown();
}

/// Two outward MCP calls attach to one actor-owned deferred reconciliation.
/// Neither call may write evidence or reply until the compacting slot emits its
/// single Killed completion; each invocation must then append its own audit row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_deferred_execution_kills_each_write_audit_evidence() {
    let controlled_slots = Arc::new(Mutex::new(HashMap::new()));
    let harness = RealPoolKillHarness::new_with_slot_factory({
        let controlled_slots = controlled_slots.clone();
        move |signal_tx| compaction_capturing_slot_factory(signal_tx, controlled_slots)
    })
    .await;
    let seeded = harness
        .seed_running_session_with_task_run("concurrent-deferred-run")
        .await;

    harness.dispatch(&seeded.task_id).await;
    harness.wait_for_runner_started(&seeded.task_id).await;
    harness.wait_for_pool_session(&seeded.task_id).await;
    let (compaction, controlled_slot) = controlled_slots
        .lock()
        .expect("controlled slot map")
        .get(&0)
        .expect("slot zero control")
        .clone();
    let guard = compaction.guard();
    assert!(
        compaction.is_compacting(),
        "fixture must hold real slot compaction before either MCP call starts"
    );

    let first = harness.call_kill_tool(&seeded.task_id);
    let second = harness.call_kill_tool(&seeded.task_id);
    let canonical_pool = harness
        .pool
        .clone()
        .try_into_djinn_slot()
        .expect("canonical pool handle");
    let attached = canonical_pool.test_wait_for_pending_reconciliation_waiters(&seeded.task_id, 2);
    tokio::pin!(first, second, attached);
    tokio::select! {
        response = &mut first => panic!("first outward kill replied before Killed: {response:?}"),
        response = &mut second => panic!("second outward kill replied before Killed: {response:?}"),
        waiter_count = &mut attached => assert_eq!(waiter_count.expect("waiter barrier"), 2),
    }
    assert!(
        controlled_slot
            .test_deferred_kill_parked()
            .await
            .expect("deferred-kill barrier"),
        "the production kill must be parked before compaction release"
    );
    assert_eq!(
        LivenessRepository::new(harness.app_state.db.clone())
            .count_evidence_for_task(&seeded.task_id)
            .await
            .expect("pre-completion evidence count"),
        0,
        "pending outward calls must not audit an unfinished reconciliation"
    );

    guard.release();
    controlled_slot
        .test_release_deferred_kill()
        .await
        .expect("release exactly one parked kill");
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first deferred outward kill");
    let second = second.expect("second deferred outward kill");
    assert_eq!(
        first, second,
        "all attached waiters receive equivalent snapshots"
    );
    assert_eq!(
        first["ok"], true,
        "released deferred reconciliation must succeed: {first}"
    );
    assert_eq!(first["kind"], "terminated");
    assert_eq!(first["task_id"], seeded.task_id);
    assert_eq!(first["observations"]["initial_compacting"], true);
    assert_eq!(
        first["observations"]["completion_source"],
        "slot_event_killed"
    );
    assert_eq!(
        first["observations"]["initial_non_terminal_ids"],
        json!([seeded.session_id.clone()])
    );
    assert_eq!(first["observations"]["final_non_terminal_ids"], json!([]));
    assert_eq!(first["executions"].as_array().map(Vec::len), Some(1));
    assert_eq!(first["executions"][0]["session_id"], seeded.session_id);
    assert_eq!(first["executions"][0]["task_run_id"], seeded.task_run_id);
    assert_eq!(first["executions"][0]["teardown_attempted"], true);
    assert_eq!(first["executions"][0]["settlement_attempted"], true);

    harness.wait_for_runner_killed(&seeded.task_id).await;
    assert_settled_after_kill(&harness, &seeded).await;
    let evidence_ids: Vec<String> =
        liveness_evidence_for_task_for_test(&harness.app_state.db, &seeded.task_id)
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();
    assert_eq!(
        evidence_ids.len(),
        2,
        "one durable row per outward invocation"
    );
    assert_ne!(evidence_ids[0], evidence_ids[1], "audit rows are distinct");
    assert_eq!(
        harness.runtime_teardown_calls(),
        vec![seeded.task_run_id.clone()],
        "attached waiters share one authoritative teardown"
    );
    harness.shutdown();
}

async fn assert_settled_after_kill(harness: &RealPoolKillHarness, seeded: &SeededRun) {
    assert!(
        !harness.pool_has_session(&seeded.task_id).await,
        "repeated kill attempts must leave no active pool session"
    );
    assert!(
        harness.running_task_ids().await.is_empty(),
        "pool status should not expose a running task after kill settlement"
    );
    assert!(
        harness.active_sessions().await.is_empty(),
        "kill settlement should leave no active DB sessions"
    );
    assert_eq!(
        harness.running_count_for_cap().await,
        0,
        "settled task must not consume per-user/model capacity"
    );
    harness
        .assert_single_terminal_session(&seeded.task_id, SessionStatus::Interrupted)
        .await;
    let session = harness.session(&seeded.session_id).await;
    assert_eq!(session.status, SessionStatus::Interrupted.as_str());
    assert!(
        session.ended_at.is_some(),
        "settled session must remain terminal after repeated kill attempts"
    );
}

fn assert_truthful_harmless_second_kill_response(response: &serde_json::Value, task_id: &str) {
    assert_eq!(response["task_id"], task_id);
    if response["ok"] == true {
        assert_eq!(
            response["error"],
            serde_json::Value::Null,
            "idempotent success should not carry an error"
        );
        return;
    }

    assert_eq!(response["ok"], false);
    let error = response["error"]
        .as_str()
        .expect("truthful second kill failure should include an error message");
    assert!(
        error.contains("no active slot")
            || error.contains("not running")
            || error.contains("not found")
            || error.contains("kill_noop"),
        "second kill should fail only because the task is already not running/not found; got {error:?}"
    );
}

struct SeededRun {
    project_id: String,
    task_id: String,
    task_run_id: String,
    session_id: String,
}

struct SeededExecution {
    session_id: String,
}

struct RealPoolKillHarness {
    harness: McpTestHarness,
    app_state: AgentContext,
    pool: SlotPoolHandle,
    runtime: RecordingRuntimeOps,
    cancel: CancellationToken,
    signal_rx: TokioMutex<mpsc::UnboundedReceiver<RunnerSignal>>,
    project_path: PathBuf,
}

impl RealPoolKillHarness {
    async fn new() -> Self {
        Self::new_with_slot_factory(|signal_tx| {
            test_slot_factory(Duration::from_secs(60), signal_tx)
        })
        .await
    }

    async fn new_with_slot_factory(
        slot_factory: impl FnOnce(mpsc::UnboundedSender<RunnerSignal>) -> SlotFactory,
    ) -> Self {
        let db = Database::open_in_memory().expect("open in-memory test database");
        let event_bus = EventBus::noop();
        let runtime = RecordingRuntimeOps::default();
        let cancel = CancellationToken::new();
        let project_path = tempfile::tempdir().expect("create project tempdir").keep();

        let app_state = AgentContext {
            db: db.clone(),
            event_bus: event_bus.clone(),
            git_actors: Arc::new(TokioMutex::new(HashMap::new())),
            background_work_tasks: Arc::new(Mutex::new(HashSet::new())),
            role_registry: Arc::new(RoleRegistry::new()),
            health_tracker: HealthTracker::new(),
            file_time: Arc::new(FileTime::new()),
            lsp: LspManager::new(),
            catalog: CatalogService::new(),
            coordinator: Arc::new(TokioMutex::new(None)),
            active_tasks: ActivityTracker::default(),
            task_ops_project_path_override: None,
            working_root: None,
            graph_warmer: None,
            repo_graph_ops: None,
            runtime_ops: Some(Arc::new(runtime.clone())),
            resize_admission: None,
            resize_drop: None,
            cargo_target_runs_root: tempfile::tempdir().expect("cargo tempdir").keep(),
            mirror: None,
            rpc_registry: None,
            default_project_id: None,
            read_source_authorization: djinn_agent::context::ReadSourceAuthorization::default(),
            memory_intent_planner: djinn_agent::context::MemoryIntentPlannerConfig::default(),
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            reconciliation_sweep: ReconciliationSweepConfig::default(),
            shell_launch: None,
            compaction_cs: djinn_slot::reply_loop::CompactionCriticalSection::default(),
        };

        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let pool = SlotPoolHandle::spawn_with_factory(
            app_state.clone(),
            cancel.clone(),
            slot_pool_config(),
            slot_factory(signal_tx),
        );

        let state = McpState::new(
            db,
            event_bus,
            CatalogService::new(),
            HealthTracker::new(),
            Some(Arc::new(StubCoordinator)),
            Some(Arc::new(RealSlotPoolBridge(pool.clone()))),
            Some(Arc::new(StubNoteEmbedding)),
            Some(Arc::new(StubNoteVectorStore)),
            Arc::new(StubLsp),
            Arc::new(runtime.clone()),
            Arc::new(StubGit),
            Arc::new(StubRepoGraph),
        );
        let harness = McpTestHarness::from_state(state);

        Self {
            harness,
            app_state,
            pool,
            runtime,
            cancel,
            signal_rx: TokioMutex::new(signal_rx),
            project_path,
        }
    }

    async fn wait_for_runner_waiting_to_complete(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rx = self.signal_rx.lock().await;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!("timed out waiting for runner completion gate for {task_id}")
                });
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(RunnerSignal::WaitingToComplete(seen))) if seen == task_id => return,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("runner signal channel closed"),
                Err(_) => panic!("timed out waiting for runner completion gate for {task_id}"),
            }
        }
    }

    async fn wait_for_runner_naturally_settled(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rx = self.signal_rx.lock().await;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!("timed out waiting for natural settlement for {task_id}")
                });
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(RunnerSignal::NaturallySettled(seen))) if seen == task_id => return,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("runner signal channel closed"),
                Err(_) => panic!("timed out waiting for natural settlement for {task_id}"),
            }
        }
    }

    async fn seed_running_session_with_task_run(&self, task_run_id: &str) -> SeededRun {
        let project =
            ProjectRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
                .create(
                    "real-pool-kill-project",
                    "test-owner",
                    "real-pool-kill-project",
                )
                .await
                .expect("project create should succeed");
        let epic = EpicRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "real-pool-kill-epic",
                    description: "test epic",
                    emoji: "🧪",
                    color: "blue",
                    owner: "test-owner",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .expect("epic create should succeed");
        let creator_key = uuid::Uuid::now_v7();
        let creator = UserRepository::new(self.app_state.db.clone())
            .upsert_from_github(
                creator_key.as_u128() as i64,
                &format!("execution-kill-fixture-{creator_key}"),
                None,
                None,
            )
            .await
            .expect("fixture user create should succeed");
        let task = TaskRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
            .create_in_project_with_provenance(
                &project.id,
                Some(&epic.id),
                EffectiveCreatorProvenance {
                    explicit_user_id: Some(&creator.id),
                    source_task_id: None,
                    proposal_id: None,
                },
                "real-pool-kill-task",
                "test task",
                "test design",
                "task",
                1,
                "test-owner",
                None,
                None,
            )
            .await
            .expect("task create should succeed");
        TaskRunRepository::new(self.app_state.db.clone())
            .create(CreateTaskRunParams {
                id: task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: "test",
                status: None,
                workspace_path: self.project_path.to_str(),
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("task_run create should succeed");
        let session =
            SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
                .create(CreateSessionParams {
                    project_id: &project.id,
                    task_id: Some(&task.id),
                    model: "model-a",
                    agent_type: "worker",
                    metadata_json: None,
                    task_run_id: Some(task_run_id),
                    pricing: None,
                    cost_basis: None,
                })
                .await
                .expect("session create should succeed");
        self.runtime.publish_reapable_taskrun_job(task_run_id);

        SeededRun {
            project_id: project.id,
            task_id: task.id,
            task_run_id: task_run_id.to_string(),
            session_id: session.id,
        }
    }

    async fn seed_duplicate_running_session(
        &self,
        seeded: &SeededRun,
        task_run_id: &str,
    ) -> SeededExecution {
        TaskRunRepository::new(self.app_state.db.clone())
            .create(CreateTaskRunParams {
                id: task_run_id,
                project_id: &seeded.project_id,
                task_id: &seeded.task_id,
                trigger_type: "test",
                status: None,
                workspace_path: self.project_path.to_str(),
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("duplicate fixture task-run create should succeed");
        let session =
            SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
                .create(CreateSessionParams {
                    project_id: &seeded.project_id,
                    task_id: Some(&seeded.task_id),
                    model: "model-a",
                    agent_type: "worker",
                    metadata_json: None,
                    task_run_id: Some(task_run_id),
                    pricing: None,
                    cost_basis: None,
                })
                .await
                .expect("duplicate fixture session create should succeed");
        self.runtime.publish_reapable_taskrun_job(task_run_id);
        SeededExecution {
            session_id: session.id,
        }
    }

    async fn seed_duplicate_session_for_existing_task_run(
        &self,
        seeded: &SeededRun,
    ) -> SeededExecution {
        let session =
            SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
                .create(CreateSessionParams {
                    project_id: &seeded.project_id,
                    task_id: Some(&seeded.task_id),
                    model: "model-a",
                    agent_type: "worker",
                    metadata_json: None,
                    task_run_id: Some(&seeded.task_run_id),
                    pricing: None,
                    cost_basis: None,
                })
                .await
                .expect("shared-run duplicate fixture session create should succeed");
        SeededExecution {
            session_id: session.id,
        }
    }

    async fn dispatch(&self, task_id: &str) {
        self.pool
            .dispatch(
                task_id,
                self.project_path.to_str().expect("utf8 path"),
                "model-a",
            )
            .await
            .expect("real pool dispatch should succeed");
    }

    async fn call_kill_tool(&self, task_id: &str) -> anyhow::Result<serde_json::Value> {
        self.harness
            .call_tool("execution_kill_task", json!({ "task_id": task_id }))
            .await
    }

    async fn pool_has_session(&self, task_id: &str) -> bool {
        self.pool
            .has_session(task_id)
            .await
            .expect("pool has_session should succeed")
    }

    async fn running_task_ids(&self) -> Vec<String> {
        let mut task_ids: Vec<_> = self
            .pool
            .get_status()
            .await
            .expect("pool status should succeed")
            .running_tasks
            .into_iter()
            .map(|task| task.task_id)
            .collect();
        task_ids.sort();
        assert_eq!(
            task_ids.iter().collect::<HashSet<_>>().len(),
            task_ids.len(),
            "pool status must not expose duplicate busy task entries: {task_ids:?}"
        );
        task_ids
    }

    async fn assert_single_terminal_session(&self, task_id: &str, expected: SessionStatus) {
        let sessions =
            SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
                .list_for_task(task_id)
                .await
                .expect("sessions for task query should succeed");
        assert_eq!(
            sessions.len(),
            1,
            "kill/completion race should leave exactly one session row for task {task_id}: {sessions:?}"
        );
        let session = &sessions[0];
        assert_eq!(
            session.status,
            expected.as_str(),
            "task {task_id} should have exactly one terminal settlement"
        );
        assert!(
            session.ended_at.is_some(),
            "terminal session for task {task_id} must have ended_at stamped"
        );
    }

    async fn assert_pool_capacity(&self, expected_active: u32, expected_free: u32) {
        let status = self
            .pool
            .get_status()
            .await
            .expect("pool status should succeed");
        let model = status
            .per_model
            .get("model-a")
            .expect("model-a status should be present");
        assert_eq!(
            (model.active, model.free),
            (expected_active, expected_free),
            "unexpected pool capacity state: {status:?}"
        );
    }

    async fn wait_for_pool_capacity(&self, expected_active: u32, expected_free: u32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = self
                .pool
                .get_status()
                .await
                .expect("pool status should succeed");
            if let Some(model) = status.per_model.get("model-a")
                && (model.active, model.free) == (expected_active, expected_free)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for model-a capacity active={expected_active} free={expected_free}; status={status:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_pool_session(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if self.pool_has_session(task_id).await {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for real pool session for {task_id}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_runner_started(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rx = self.signal_rx.lock().await;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for runner start for {task_id}"));
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(RunnerSignal::Started(seen))) if seen == task_id => return,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("runner signal channel closed"),
                Err(_) => panic!("timed out waiting for runner start for {task_id}"),
            }
        }
    }

    async fn wait_for_runner_killed(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rx = self.signal_rx.lock().await;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for runner kill for {task_id}"));
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(RunnerSignal::Killed(seen))) if seen == task_id => return,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("runner signal channel closed"),
                Err(_) => panic!("timed out waiting for runner kill for {task_id}"),
            }
        }
    }

    fn runtime_teardown_calls(&self) -> Vec<String> {
        self.runtime.teardown_calls()
    }

    async fn session(&self, session_id: &str) -> djinn_core::models::SessionRecord {
        SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
            .get(session_id)
            .await
            .expect("session lookup should succeed")
            .expect("session should exist")
    }

    async fn active_sessions(&self) -> Vec<djinn_core::models::SessionRecord> {
        SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
            .list_active()
            .await
            .expect("active session query should succeed")
    }

    async fn running_count_for_cap(&self) -> i64 {
        SessionRepository::new(self.app_state.db.clone(), self.app_state.event_bus.clone())
            .count_active_by_user_and_model()
            .await
            .expect("running cap count should succeed")
            .into_iter()
            .map(|(_user, _model, count)| count)
            .sum()
    }

    fn shutdown(&self) {
        self.cancel.cancel();
    }
}

fn slot_pool_config() -> SlotPoolConfig {
    SlotPoolConfig {
        models: vec![ModelSlotConfig {
            model_id: "model-a".to_string(),
            max_slots: 1,
            roles: HashSet::from(["worker".to_string()]),
        }],
        role_priorities: HashMap::from([("worker".to_string(), vec!["model-a".to_string()])]),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunnerSignal {
    Started(String),
    WaitingToComplete(String),
    NaturallySettled(String),
    Killed(String),
}

#[derive(Clone, Default)]
struct CompletionRaceControl {
    allow_natural_settlement: Arc<Notify>,
}

impl CompletionRaceControl {
    fn allow_natural_settlement(&self) {
        self.allow_natural_settlement.notify_waiters();
    }
}

fn compaction_capturing_slot_factory(
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
    controlled_slots: Arc<
        Mutex<
            HashMap<
                usize,
                (
                    djinn_slot::reply_loop::CompactionCriticalSection,
                    djinn_slot::SlotHandle,
                ),
            >,
        >,
    >,
) -> SlotFactory {
    let factory = test_slot_factory(Duration::from_secs(3600), signal_tx);
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let handle = factory(slot_id, model_id, event_tx, app_state, cancel);
        controlled_slots
            .lock()
            .expect("controlled slot map")
            .insert(
                slot_id,
                (
                    handle.test_compaction_cs().clone(),
                    handle.clone().into_djinn_slot(),
                ),
            );
        handle
    })
}

fn test_slot_factory(
    runtime: Duration,
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
) -> SlotFactory {
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let signal_tx = signal_tx.clone();
        let runner: djinn_agent::actors::slot::TestLifecycleRunner = Arc::new(
            move |task_id,
                  _execution_generation,
                  _project_path,
                  _model_id,
                  _app_state,
                  kill,
                  _pause,
                  _resume_lifecycle_metadata| {
                let signal_tx = signal_tx.clone();
                Box::pin(async move {
                    let _ = signal_tx.send(RunnerSignal::Started(task_id.clone()));
                    tokio::select! {
                        _ = tokio::time::sleep(runtime) => {}
                        _ = kill.cancelled() => {
                            let _ = signal_tx.send(RunnerSignal::Killed(task_id));
                        }
                    }
                    Ok(())
                })
            },
        );

        SlotHandle::spawn_with_test_runner(slot_id, model_id, event_tx, app_state, cancel, runner)
    })
}

#[derive(Clone, Default)]
struct RecordingRuntimeOps {
    teardown_calls: Arc<Mutex<Vec<String>>>,
    taskrun_jobs: Arc<Mutex<Vec<djinn_control_plane::bridge::TaskrunJobRef>>>,
}

impl RecordingRuntimeOps {
    fn teardown_calls(&self) -> Vec<String> {
        self.teardown_calls
            .lock()
            .expect("teardown calls mutex")
            .clone()
    }

    /// Publish a Job whose terminal evidence sits at the Unix epoch, so the
    /// shared `djinn_core::job_retention` classifier has long since passed the
    /// failure retention window under the real clock. Without an inventory
    /// entry the cleanup paths fail closed and preserve the Job, which would
    /// make every teardown assertion below vacuous.
    fn publish_reapable_taskrun_job(&self, task_run_id: &str) {
        self.taskrun_jobs.lock().expect("runtime jobs mutex").push(
            djinn_control_plane::bridge::TaskrunJobRef {
                job_name: format!("djinn-taskrun-{task_run_id}"),
                task_run_id: task_run_id.to_string(),
                created_at: Some(std::time::SystemTime::UNIX_EPOCH),
                completed_at: Some(std::time::SystemTime::UNIX_EPOCH),
                terminal_condition: Some("Failed".to_string()),
            },
        );
    }
}

#[async_trait]
impl RuntimeOps for RecordingRuntimeOps {
    async fn apply_settings(&self, _settings: &DjinnSettings) -> Result<(), String> {
        Ok(())
    }

    async fn embed_memory_query(
        &self,
        _query: &str,
    ) -> Result<Option<djinn_control_plane::bridge::SemanticQueryEmbedding>, String> {
        Ok(None)
    }

    async fn reset_runtime_settings(&self) {}

    async fn persist_model_health_state(&self) {}

    async fn apply_environment_config(
        &self,
        _project_id: &str,
        _config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn trigger_mirror_refresh(&self, _project_id: &str) {}

    async fn enqueue_image_build(&self, _image_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn trigger_graph_warm(&self, _project_id: &str) {}

    async fn apply_user_model_change(&self) {}

    async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), String> {
        self.teardown_calls
            .lock()
            .expect("teardown calls mutex")
            .push(task_run_id.to_string());
        Ok(())
    }

    async fn list_taskrun_jobs(
        &self,
    ) -> Result<Vec<djinn_control_plane::bridge::TaskrunJobRef>, String> {
        Ok(self
            .taskrun_jobs
            .lock()
            .expect("runtime jobs mutex")
            .clone())
    }

    async fn cleanup_task_branches(&self, _task_id: &str) {}
}

struct RealSlotPoolBridge(SlotPoolHandle);

#[async_trait]
impl SlotPoolOps for RealSlotPoolBridge {
    async fn get_status(&self) -> Result<PoolStatus, String> {
        let status = self.0.get_status().await.map_err(|e| e.to_string())?;
        Ok(PoolStatus {
            active_slots: status.active_slots,
            total_slots: status.total_slots,
            per_model: status
                .per_model
                .into_iter()
                .map(|(model, status)| {
                    (
                        model,
                        ModelPoolStatus {
                            active: status.active,
                            free: status.free,
                            total: status.total,
                        },
                    )
                })
                .collect(),
            running_tasks: status
                .running_tasks
                .into_iter()
                .map(|task| RunningTaskInfo {
                    task_id: task.task_id,
                    model_id: task.model_id,
                    slot_id: task.slot_id,
                    duration_seconds: task.duration_seconds,
                    idle_seconds: task.idle_seconds,
                    project_id: task.project_id,
                    no_progress_streak: task.no_progress_streak,
                })
                .collect(),
        })
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), String> {
        self.0
            .kill_session(task_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn terminate_session(&self, task_id: &str) -> Result<(), String> {
        self.0
            .terminate_session(task_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn reconcile_terminate(
        &self,
        task_id: &str,
    ) -> Result<ReconcileTerminateSnapshot, String> {
        let snapshot = self
            .0
            .reconcile_terminate(task_id)
            .await
            .map_err(|e| e.to_string())?;
        let kind = |kind| match kind {
            djinn_slot::pool::ReconcileTerminateKind::GenuinelyAbsent => {
                ReconcileTerminateKind::GenuinelyAbsent
            }
            djinn_slot::pool::ReconcileTerminateKind::Terminated => {
                ReconcileTerminateKind::Terminated
            }
            djinn_slot::pool::ReconcileTerminateKind::DesyncReconciled => {
                ReconcileTerminateKind::DesyncReconciled
            }
            djinn_slot::pool::ReconcileTerminateKind::TeardownFailed => {
                ReconcileTerminateKind::TeardownFailed
            }
            djinn_slot::pool::ReconcileTerminateKind::SettlementFailed => {
                ReconcileTerminateKind::SettlementFailed
            }
            djinn_slot::pool::ReconcileTerminateKind::ReconciliationIncomplete => {
                ReconcileTerminateKind::ReconciliationIncomplete
            }
        };
        Ok(ReconcileTerminateSnapshot {
            ok: snapshot.ok,
            kind: kind(snapshot.kind),
            task_id: snapshot.task_id,
            executions: snapshot
                .executions
                .into_iter()
                .map(|e| ReconcileTerminateExecution {
                    session_id: e.session_id,
                    task_run_id: e.task_run_id,
                    teardown_owner: e.teardown_owner,
                    teardown_attempted: e.teardown_attempted,
                    teardown_error: e.teardown_error,
                    settlement_attempted: e.settlement_attempted,
                    settlement_error: e.settlement_error,
                })
                .collect(),
            observations: ReconcileTerminateObservations {
                initial_non_terminal_ids: snapshot.observations.initial_non_terminal_ids,
                initial_mapping_slot_id: snapshot.observations.initial_mapping_slot_id,
                initial_pending_teardown: snapshot.observations.initial_pending_teardown,
                initial_compacting: snapshot.observations.initial_compacting,
                fenced_generation: snapshot.observations.fenced_generation,
                initial_capture_error: snapshot.observations.initial_capture_error,
                final_non_terminal_ids: snapshot.observations.final_non_terminal_ids,
                final_mapping_slot_id: snapshot.observations.final_mapping_slot_id,
                final_pending_teardown: snapshot.observations.final_pending_teardown,
                final_reread_error: snapshot.observations.final_reread_error,
                pool_cleanup_error: snapshot.observations.pool_cleanup_error,
                completion_source: snapshot.observations.completion_source,
                underlying_kind: snapshot.observations.underlying_kind.map(kind),
            },
        })
    }

    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String> {
        self.0
            .session_for_task(task_id)
            .await
            .map_err(|e| e.to_string())
            .map(|task| {
                task.map(|task| RunningTaskInfo {
                    task_id: task.task_id,
                    model_id: task.model_id,
                    slot_id: task.slot_id,
                    duration_seconds: task.duration_seconds,
                    idle_seconds: task.idle_seconds,
                    project_id: task.project_id,
                    no_progress_streak: task.no_progress_streak,
                })
            })
    }

    async fn has_session(&self, task_id: &str) -> Result<bool, String> {
        self.0.has_session(task_id).await.map_err(|e| e.to_string())
    }
}
