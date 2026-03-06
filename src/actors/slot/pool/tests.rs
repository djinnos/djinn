use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::sync::mpsc;

use super::*;
use crate::agent::init_session_manager;
use crate::test_helpers;

use super::super::{ModelSlotConfig, SlotHandle, SlotPoolConfig};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunnerSignal {
    Started(String),
    Completed(String),
    Killed(String),
    Paused(String),
}

fn test_app_state() -> (crate::server::AppState, Arc<crate::agent::SessionManager>, tokio_util::sync::CancellationToken, TempDir) {
    let db = test_helpers::create_test_db();
    let cancel = tokio_util::sync::CancellationToken::new();
    let app_state = crate::server::AppState::new(db, cancel.clone());
    let temp = tempfile::tempdir().expect("tempdir");
    let session_manager = init_session_manager(temp.path().to_path_buf());
    (app_state, session_manager, cancel, temp)
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
    Arc::new(
        move |slot_id, model_id, event_tx, app_state, session_manager, cancel| {
            let signal_tx = signal_tx.clone();
            let runner: super::super::actor::TestLifecycleRunner = Arc::new(
                move |task_id,
                      _project_path,
                      _model_id,
                      _app_state,
                      _session_manager,
                      kill,
                      pause| {
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

            SlotHandle::spawn_with_test_runner(
                slot_id,
                model_id,
                event_tx,
                app_state,
                session_manager,
                cancel,
                runner,
            )
        },
    )
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

#[tokio::test]
async fn parallel_completions_finish_concurrently() {
    let (app_state, session_manager, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 4, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        session_manager,
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

#[tokio::test]
async fn model_priority_fallback_uses_next_model_when_primary_full() {
    let (app_state, session_manager, cancel, _temp) = test_app_state();
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
        session_manager,
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

    assert_eq!(m1, "model-a");
    assert_eq!(m2, "model-b");
    assert_eq!(m3, "model-b");

    let fourth = dispatch_for_role(
        &pool,
        "task-4",
        "/tmp/project",
        "worker",
        &role_priorities,
        &model_roles,
    )
    .await;
    assert!(matches!(fourth, Err(PoolError::AtCapacity { .. })));

    pool.interrupt_all("test cleanup")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(&pool, &["task-1".into(), "task-2".into(), "task-3".into()]).await;
}

#[tokio::test]
async fn role_isolation_skips_models_that_do_not_serve_role() {
    let (app_state, session_manager, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![
            model("opus", 1, &["task_reviewer"]),
            model("sonnet", 1, &["worker"]),
        ],
        &[
            ("worker", vec!["opus", "sonnet"]),
            ("task_reviewer", vec!["opus"]),
        ],
    );
    let role_priorities = config.role_priorities.clone();
    let model_roles: HashMap<String, HashSet<String>> = HashMap::from([
        ("opus".to_string(), role_set(&["task_reviewer"])),
        ("sonnet".to_string(), role_set(&["worker"])),
    ]);

    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        session_manager,
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
    assert!(matches!(second, Err(PoolError::AtCapacity { .. })));

    pool.interrupt_all("test cleanup")
        .await
        .expect("interrupt_all should succeed");
    wait_until_no_sessions(&pool, &["worker-1".into()]).await;
}

#[tokio::test]
async fn reconfigure_scale_up_adds_free_slots_for_dispatch() {
    let (app_state, session_manager, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 2, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        session_manager,
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
    assert!(matches!(
        pool.dispatch("up-3", "/tmp/project", "model-a").await,
        Err(PoolError::AtCapacity { .. })
    ));

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

#[tokio::test]
async fn reconfigure_scale_down_drains_busy_slots_then_retires_them() {
    let (app_state, session_manager, cancel, _temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 4, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        session_manager,
        cancel,
        config,
        test_slot_factory(Duration::from_millis(500), signal_tx),
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

    let status_during_drain = pool.get_status().await.expect("status should succeed");
    assert_eq!(status_during_drain.total_slots, 4);

    wait_until_no_sessions(&pool, &task_ids).await;

    let status_after = pool.get_status().await.expect("status should succeed");
    assert_eq!(status_after.total_slots, 2);

    pool.dispatch("down-next-1", "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    pool.dispatch("down-next-2", "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    assert!(matches!(
        pool.dispatch("down-next-3", "/tmp/project", "model-a")
            .await,
        Err(PoolError::AtCapacity { .. })
    ));
}

#[tokio::test]
async fn kill_and_pause_are_routed_to_the_correct_task_slot() {
    let (app_state, session_manager, cancel, _temp) = test_app_state();
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 2, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        session_manager,
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
