use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::sync::mpsc;

use super::*;
use crate::test_helpers;

use super::super::{ModelSlotConfig, SlotHandle, SlotPoolConfig};
use super::actor::SlotPool;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunnerSignal {
    Started(String),
    Completed(String),
    Killed(String),
    Paused(String),
}

fn test_app_state() -> (
    crate::context::AgentContext,
    tokio_util::sync::CancellationToken,
    TempDir,
) {
    let db = test_helpers::create_test_db();
    let cancel = tokio_util::sync::CancellationToken::new();
    let app_state = test_helpers::agent_context_from_db(db, cancel.clone());
    let temp = test_helpers::test_tempdir("djinn-slot-pool-");
    (app_state, cancel, temp)
}

fn model(model_id: &str, max_slots: u32, roles: &[&str]) -> ModelSlotConfig {
    ModelSlotConfig {
        model_id: model_id.to_string(),
        max_slots,
        roles: roles.iter().map(|r| (*r).to_string()).collect(),
    }
}

fn role_set(roles: &[&str]) -> HashSet<String> {
    roles.iter().map(|r| (*r).to_string()).collect()
}

fn make_config(
    models: Vec<ModelSlotConfig>,
    role_priorities: &[(&str, Vec<&str>)],
) -> SlotPoolConfig {
    SlotPoolConfig {
        models,
        role_priorities: role_priorities
            .iter()
            .map(|(role, priorities)| {
                (
                    (*role).to_string(),
                    priorities.iter().map(|m| (*m).to_string()).collect(),
                )
            })
            .collect(),
    }
}

fn test_slot_factory(
    runtime: Duration,
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
) -> SlotFactory {
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let signal_tx = signal_tx.clone();
        let runner: super::super::actor::TestLifecycleRunner = Arc::new(
            move |task_id, _project_path, _model_id, _app_state, kill, pause| {
                let signal_tx = signal_tx.clone();
                Box::pin(async move {
                    let _ = signal_tx.send(RunnerSignal::Started(task_id.clone()));
                    tokio::select! {
                        _ = tokio::time::sleep(runtime) => {
                            let _ = signal_tx.send(RunnerSignal::Completed(task_id));
                        }
                        _ = kill.cancelled() => {
                            let _ = signal_tx.send(RunnerSignal::Killed(task_id));
                        }
                        _ = pause.cancelled() => {
                            let _ = signal_tx.send(RunnerSignal::Paused(task_id));
                        }
                    }
                    Ok(())
                })
            },
        );

        SlotHandle::spawn_with_test_runner(slot_id, model_id, event_tx, app_state, cancel, runner)
    })
}

async fn wait_until_no_sessions(pool: &SlotPoolHandle, task_ids: &[String]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut any_running = false;
        for task_id in task_ids {
            if pool
                .has_session(task_id)
                .await
                .expect("has_session should succeed")
            {
                any_running = true;
                break;
            }
        }
        if !any_running {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for sessions to clear"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn dispatch_for_role(
    pool: &SlotPoolHandle,
    task_id: &str,
    project_path: &str,
    role: &str,
    role_priorities: &HashMap<String, Vec<String>>,
    model_roles: &HashMap<String, HashSet<String>>,
) -> Result<String, PoolError> {
    let priorities = role_priorities.get(role).cloned().unwrap_or_default();

    let mut last_capacity: Option<PoolError> = None;
    for model_id in priorities {
        if !model_roles
            .get(&model_id)
            .is_some_and(|roles| roles.contains(role))
        {
            continue;
        }

        match pool.dispatch(task_id, project_path, &model_id).await {
            Ok(()) => return Ok(model_id),
            Err(PoolError::AtCapacity { .. }) => {
                last_capacity = Some(PoolError::AtCapacity {
                    model_id: model_id.clone(),
                });
            }
            Err(other) => return Err(other),
        }
    }

    Err(last_capacity.unwrap_or(PoolError::AtCapacity {
        model_id: role.to_string(),
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_completions_finish_concurrently() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 4, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_millis(120), signal_tx),
    );

    let task_ids: Vec<String> = (0..4).map(|i| format!("parallel-{i}")).collect();
    for task_id in &task_ids {
        pool.dispatch(task_id, "/tmp/project", "model-a")
            .await
            .expect("dispatch should succeed");
    }

    let started = Instant::now();
    wait_until_no_sessions(&pool, &task_ids).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(380),
        "expected concurrent completion under 380ms, got {:?}",
        elapsed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elastic_pool_spawns_on_demand_without_capacity_fallback() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![
            model("model-a", 1, &["worker"]),
            model("model-b", 2, &["worker"]),
        ],
        &[("worker", vec!["model-a", "model-b"])],
    );
    let role_priorities = config.role_priorities.clone();
    let model_roles: HashMap<String, HashSet<String>> = HashMap::from([
        ("model-a".to_string(), role_set(&["worker"])),
        ("model-b".to_string(), role_set(&["worker"])),
    ]);

    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );

    let m1 = dispatch_for_role(
        &pool,
        "task-1",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await
    .expect("first dispatch should succeed");
    let m2 = dispatch_for_role(
        &pool,
        "task-2",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await
    .expect("second dispatch should succeed");
    let m3 = dispatch_for_role(
        &pool,
        "task-3",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await
    .expect("third dispatch should succeed");

    // Elastic pool: there is no per-model ceiling — repeated dispatches to the
    // first-priority model all spawn on demand (past the pre-warm count), so we
    // never fall back to model-b and never hit AtCapacity. Per-user concurrency
    // is now enforced by the coordinator's per-(user,model) gate, not the pool.
    assert_eq!(m1, "model-a");
    assert_eq!(m2, "model-a");
    assert_eq!(m3, "model-a");

    let fourth = dispatch_for_role(
        &pool,
        "task-4",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await;
    assert_eq!(
        fourth.expect("elastic dispatch always succeeds (spawns on demand)"),
        "model-a"
    );

    pool.interrupt_all("test cleanup")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(
        &pool,
        &[
            "task-1".into(),
            "task-2".into(),
            "task-3".into(),
            "task-4".into(),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_isolation_skips_models_that_do_not_serve_role() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![
            model("opus", 1, &["reviewer"]),
            model("sonnet", 1, &["worker"]),
        ],
        &[
            ("worker", vec!["opus", "sonnet"]),
            ("reviewer", vec!["opus"]),
        ],
    );
    let role_priorities = config.role_priorities.clone();
    let model_roles: HashMap<String, HashSet<String>> = HashMap::from([
        ("opus".to_string(), role_set(&["reviewer"])),
        ("sonnet".to_string(), role_set(&["worker"])),
    ]);

    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );

    let first = dispatch_for_role(
        &pool,
        "worker-1",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await
    .expect("worker dispatch should succeed");
    assert_eq!(first, "sonnet");

    let status = pool.get_status().await.expect("status should succeed");
    assert_eq!(status.per_model.get("opus").map(|s| s.free), Some(1));

    let second = dispatch_for_role(
        &pool,
        "worker-2",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await;
    // Elastic: sonnet spawns a second slot on demand. The point of this test is
    // role isolation — opus (reviewer-only) is skipped, so both worker tasks
    // land on sonnet, never opus.
    assert_eq!(
        second.expect("elastic dispatch succeeds on the worker-serving model"),
        "sonnet"
    );
    let status = pool.get_status().await.expect("status should succeed");
    assert_eq!(
        status.per_model.get("opus").map(|s| s.free),
        Some(1),
        "opus (reviewer-only) must stay untouched by worker dispatches"
    );

    pool.interrupt_all("test cleanup")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(&pool, &["worker-1".into(), "worker-2".into()]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_scale_up_adds_free_slots_for_dispatch() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 2, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );

    pool.dispatch("up-1", "/tmp/project", "model-a")
        .await
        .expect("dispatch 1 should succeed");
    pool.dispatch("up-2", "/tmp/project", "model-a")
        .await
        .expect("dispatch 2 should succeed");
    // (No AtCapacity check at the pre-warm count of 2 — the pool is elastic.
    // This test now verifies that scaling UP pre-warms additional FREE slots.)

    pool.reconfigure(make_config(
        vec![model("model-a", 4, &["worker"])],
        &[("worker", vec!["model-a"])],
    ))
    .await
    .expect("reconfigure should succeed");

    let status = pool.get_status().await.expect("status should succeed");
    let per_model = status
        .per_model
        .get("model-a")
        .expect("model-a should exist in status");
    assert_eq!(status.total_slots, 4);
    assert_eq!(per_model.active, 2);
    assert_eq!(per_model.free, 2);

    pool.dispatch("up-3", "/tmp/project", "model-a")
        .await
        .expect("dispatch 3 should succeed after scale-up");
    pool.dispatch("up-4", "/tmp/project", "model-a")
        .await
        .expect("dispatch 4 should succeed after scale-up");

    pool.interrupt_all("test cleanup")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(
        &pool,
        &["up-1".into(), "up-2".into(), "up-3".into(), "up-4".into()],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfigure_scale_down_drains_busy_slots_then_retires_them() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 4, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );

    let task_ids: Vec<String> = (0..4).map(|i| format!("down-{i}")).collect();
    for task_id in &task_ids {
        pool.dispatch(task_id, "/tmp/project", "model-a")
            .await
            .expect("dispatch should succeed");
    }

    pool.reconfigure(make_config(
        vec![model("model-a", 2, &["worker"])],
        &[("worker", vec!["model-a"])],
    ))
    .await
    .expect("reconfigure should succeed");

    // Tasks are still running (10s runtime), so all 4 slots should still exist.
    let status_during_drain = pool.get_status().await.expect("status should succeed");
    assert_eq!(status_during_drain.total_slots, 4);

    // Kill all tasks so they finish immediately.
    pool.interrupt_all("test drain")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(&pool, &task_ids).await;

    // Scale-down still drained the idle slots back to the pre-warm count of 2.
    let status_after = pool.get_status().await.expect("status should succeed");
    assert_eq!(status_after.total_slots, 2);

    // Elastic: the 2 retained slots are reused, and a 3rd dispatch spawns a new
    // slot on demand — there is no per-model capacity ceiling anymore.
    pool.dispatch("down-next-1", "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    pool.dispatch("down-next-2", "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    pool.dispatch("down-next-3", "/tmp/project", "model-a")
        .await
        .expect("elastic dispatch spawns a slot on demand");
    pool.interrupt_all("test cleanup")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(
        &pool,
        &[
            "down-next-1".into(),
            "down-next-2".into(),
            "down-next-3".into(),
        ],
    )
    .await;
}

/// Sum the per-(user, model) running-session counts the coordinator uses to
/// enforce the per-user concurrency cap. A settled (terminal) row must not
/// appear here.
async fn running_count_for_cap(repo: &djinn_db::SessionRepository) -> i64 {
    repo.count_active_by_user_and_model()
        .await
        .expect("count_active_by_user_and_model should succeed")
        .into_iter()
        .map(|(_creator, _model, cnt)| cnt)
        .sum()
}

/// D4: a stall-kill (any `SlotEvent::Killed` path) must SETTLE the session DB
/// row to a terminal state at the moment of the kill — not leave it `running`
/// until the periodic zombie backstop. A lingering `running` row over-counts
/// the per-user concurrency cap (fatal at `max_sessions = 1`: the user can't
/// redispatch because a dead session still "counts"). Re-settling an
/// already-terminal row is a no-op (idempotent).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_kill_settles_session_row_and_clears_from_per_user_cap() {
    use djinn_db::{EpicCreateInput, EpicRepository, TaskRepository};

    let (app_state, cancel, _temp) = test_app_state();
    let db = app_state.db.clone();
    let event_bus = app_state.event_bus.clone();
    let session_repo = djinn_db::SessionRepository::new(db.clone(), event_bus.clone());

    // Seed a real project → epic → task so the session row satisfies its
    // FK constraints (`sessions.project_id` → projects, `sessions.task_id`
    // → tasks).
    let project = test_helpers::create_test_project(&db).await;
    let epic = EpicRepository::new(db.clone(), event_bus.clone())
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "Epic",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
            },
        )
        .await
        .expect("epic create should succeed");
    let task = TaskRepository::new(db.clone(), event_bus.clone())
        .create(&epic.id, "stall-kill", "", "", "task", 0, "", Some("open"))
        .await
        .expect("task create should succeed");
    let task_id = task.id.as_str();

    // Materialize the `running` session row the real worker lifecycle would
    // create. (The test slot runner is a stub and does not touch the DB.)
    let created = session_repo
        .create(djinn_db::CreateSessionParams {
            project_id: &project.id,
            task_id: Some(task_id),
            model: "model-a",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
        })
        .await
        .expect("session create should succeed");
    assert_eq!(
        created.status,
        djinn_core::models::SessionStatus::Running.as_str(),
        "session starts in the running state"
    );
    assert_eq!(
        running_count_for_cap(&session_repo).await,
        1,
        "a running worker session counts against the per-user cap"
    );

    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 1, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );

    pool.dispatch(task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");

    // Stall-kill: the coordinator's stall detector routes through
    // `pool.kill_session`, which kills the slot and emits `SlotEvent::Killed`.
    pool.kill_session(task_id)
        .await
        .expect("kill_session should succeed");

    // Wait for the slot to drain (the `Killed` event handler runs the settle).
    wait_until_no_sessions(&pool, &[task_id.to_string()]).await;

    // The settle is performed in `handle_slot_event` after the kill event; poll
    // until the row goes terminal (the event is processed async).
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let row = session_repo
            .get(&created.id)
            .await
            .expect("get should succeed")
            .expect("session row should still exist");
        if row.status != djinn_core::models::SessionStatus::Running.as_str() {
            assert_eq!(
                row.status,
                djinn_core::models::SessionStatus::Interrupted.as_str(),
                "stall-killed session must settle to a terminal (interrupted) state"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stall-killed session row was never settled (still running)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The settled row no longer counts against the per-user concurrency cap.
    assert_eq!(
        running_count_for_cap(&session_repo).await,
        0,
        "a settled (terminal) session must NOT count against the per-user cap"
    );
    assert!(
        session_repo
            .list_active()
            .await
            .expect("list_active should succeed")
            .is_empty(),
        "no running sessions should remain after a stall-kill"
    );

    // Idempotent: re-settling an already-terminal row affects zero rows and
    // leaves it terminal.
    let reaffected = session_repo
        .interrupt_running_for_task(task_id)
        .await
        .expect("re-settle should succeed");
    assert_eq!(
        reaffected, 0,
        "re-settling an already-terminal row must be a no-op"
    );
    let row = session_repo
        .get(&created.id)
        .await
        .expect("get should succeed")
        .expect("session row should still exist");
    assert_eq!(
        row.status,
        djinn_core::models::SessionStatus::Interrupted.as_str(),
        "re-settle leaves the row terminal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_and_pause_are_routed_to_the_correct_task_slot() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 2, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );

    pool.dispatch("task-kill", "/tmp/project", "model-a")
        .await
        .expect("kill task dispatch should succeed");
    pool.dispatch("task-pause", "/tmp/project", "model-a")
        .await
        .expect("pause task dispatch should succeed");

    let kill_slot = pool
        .session_for_task("task-kill")
        .await
        .expect("session lookup should succeed")
        .expect("kill task should have active session")
        .slot_id;
    let pause_slot = pool
        .session_for_task("task-pause")
        .await
        .expect("session lookup should succeed")
        .expect("pause task should have active session")
        .slot_id;
    assert_ne!(
        kill_slot, pause_slot,
        "tasks should be running in different slots"
    );

    pool.kill_session("task-kill")
        .await
        .expect("kill should succeed");
    pool.pause_session("task-pause")
        .await
        .expect("pause should succeed");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_kill = false;
    let mut saw_pause = false;
    while !(saw_kill && saw_pause) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for kill/pause signals"
        );
        if let Some(signal) = tokio::time::timeout(Duration::from_millis(200), signal_rx.recv())
            .await
            .expect("signal read should not timeout")
        {
            match signal {
                RunnerSignal::Killed(task_id) if task_id == "task-kill" => saw_kill = true,
                RunnerSignal::Paused(task_id) if task_id == "task-pause" => saw_pause = true,
                _ => {}
            }
        }
    }

    wait_until_no_sessions(&pool, &["task-kill".into(), "task-pause".into()]).await;
}

/// Regression: a slot that is still busy must never wedge dispatch even if it
/// wrongly reappears on the free list. Before the fix, `evict_session` pushed an
/// evicted slot back to `free_slots` while its actor was still winding down (and
/// the Killed event pushed it a second time); `dispatch` then popped that stale
/// entry, got `SlotBusy`, re-queued it, and spun forever — every task for the
/// model stuck on "slot is busy", never spawning a fresh slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_recovers_from_stale_busy_free_slot() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 1, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let (_pool_tx, pool_rx) = mpsc::channel(8);
    let mut pool = SlotPool::new_with_factory(
        pool_rx,
        app_state,
        cancel,
        config,
        // Long runtime: the slot stays genuinely busy for the whole test.
        test_slot_factory(Duration::from_secs(3600), signal_tx),
    );

    pool.test_dispatch("task-a", "/tmp/project", "model-a")
        .await
        .expect("first dispatch should occupy a slot");
    let slot_a = pool
        .test_slot_of("task-a")
        .expect("task-a should hold a slot");

    // Inject the exact desync: the still-busy slot back on the free list.
    pool.test_inject_free(slot_a, "model-a");

    // Must self-heal: drop the stale entry and spawn a fresh slot — NOT wedge.
    pool.test_dispatch("task-b", "/tmp/project", "model-a")
        .await
        .expect("second dispatch must recover instead of wedging on the busy slot");
    let slot_b = pool
        .test_slot_of("task-b")
        .expect("task-b should hold a slot");

    assert_ne!(
        slot_b, slot_a,
        "task-b must land on a fresh slot, not the still-busy one"
    );
    assert!(
        !pool.test_free_slots("model-a").contains(&slot_a),
        "the stale busy slot must be dropped from the free list, not re-queued"
    );
}

/// `mark_slot_free` is the single authoritative free-list append: it must never
/// create a duplicate entry and must never resurrect a retired slot. A duplicate
/// is precisely what hands a busy slot to a later task (`SlotBusy` → wedge), and
/// a resurrected retired slot would route work to a drained worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_slot_free_is_idempotent_and_skips_retired() {
    let (app_state, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 1, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let (_pool_tx, pool_rx) = mpsc::channel(8);
    let mut pool = SlotPool::new_with_factory(
        pool_rx,
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(3600), signal_tx),
    );

    // spawn_slots_for_config already created slot 0, free exactly once.
    assert_eq!(pool.test_free_slots("model-a"), vec![0]);

    // Re-freeing an already-free slot is a no-op, never a duplicate.
    pool.test_mark_slot_free(0, "model-a");
    assert_eq!(
        pool.test_free_slots("model-a"),
        vec![0],
        "mark_slot_free must not duplicate an already-free slot"
    );

    // Take slot 0 out of the free list (as a dispatch would), then retire it.
    pool.test_dispatch("task-a", "/tmp/project", "model-a")
        .await
        .expect("dispatch should occupy slot 0");
    assert_eq!(pool.test_slot_of("task-a"), Some(0));
    assert!(!pool.test_free_slots("model-a").contains(&0));
    pool.test_retire(0);

    // A stale Free event for a retired slot must not return it to rotation.
    pool.test_mark_slot_free(0, "model-a");
    assert!(
        !pool.test_free_slots("model-a").contains(&0),
        "mark_slot_free must refuse to resurrect a retired slot"
    );
}
