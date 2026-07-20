// djinn:allow-oversize — integration tests for slot-pool actor behavior.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};

use super::*;
use crate::test_helpers;

use super::super::{ModelSlotConfig, SlotEvent, SlotHandle, SlotPoolConfig, SlotState};
use super::actor::SlotPool;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunnerSignal {
    Started(String),
    Completed(String),
    Killed(String),
    Paused(String),
}

fn test_app_state() -> (
    crate::host::SlotContext,
    tokio_util::sync::CancellationToken,
    TempDir,
) {
    let db = test_helpers::create_test_db();
    let cancel = tokio_util::sync::CancellationToken::new();
    let app_state = test_helpers::agent_context_from_db(db, cancel.clone());
    let temp = test_helpers::test_tempdir("djinn-slot-pool-");
    (app_state, cancel, temp)
}

#[derive(Clone)]
struct RecordingRuntimeOps {
    calls: Arc<Mutex<Vec<String>>>,
    fail_teardown: bool,
}

impl RecordingRuntimeOps {
    fn new(fail_teardown: bool) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_teardown,
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls mutex").clone()
    }
}

#[async_trait::async_trait]
impl djinn_control_plane::bridge::RuntimeOps for RecordingRuntimeOps {
    async fn apply_settings(&self, _: &djinn_core::models::DjinnSettings) -> Result<(), String> {
        Ok(())
    }
    async fn embed_memory_query(
        &self,
        _: &str,
    ) -> Result<Option<djinn_control_plane::bridge::SemanticQueryEmbedding>, String> {
        Ok(None)
    }
    async fn reset_runtime_settings(&self) {}
    async fn persist_model_health_state(&self) {}
    async fn apply_environment_config(
        &self,
        _: &str,
        _: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_mirror_refresh(&self, _: &str) {}
    async fn apply_user_model_change(&self) {}
    async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    async fn trigger_graph_warm(&self, _: &str) {}
    async fn teardown_taskrun_job(&self, task_run_id: &str) -> Result<(), String> {
        self.calls
            .lock()
            .expect("calls mutex")
            .push(task_run_id.to_string());
        if self.fail_teardown {
            Err("synthetic teardown failure".to_string())
        } else {
            Ok(())
        }
    }
    async fn list_taskrun_jobs(
        &self,
    ) -> Result<Vec<djinn_control_plane::bridge::TaskrunJobRef>, String> {
        // Agent-internal test fakes don't track a kube inventory.
        Ok(Vec::new())
    }
    async fn cleanup_task_branches(&self, _: &str) {}
}

async fn seed_running_session_with_task_run(
    app_state: &crate::host::SlotContext,
    _task_title: &str,
    task_run_id: &str,
) -> String {
    let project = test_helpers::create_test_project(&app_state.db).await;
    let epic = test_helpers::create_test_epic(&app_state.db, &project.id).await;
    let task = test_helpers::create_test_task(&app_state.db, &project.id, &epic.id).await;
    let task_id = task.id.clone();
    djinn_db::TaskRunRepository::new(app_state.db.clone())
        .create(djinn_db::CreateTaskRunParams {
            id: task_run_id,
            project_id: &project.id,
            task_id: &task_id,
            trigger_type: "test",
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("task_run create should succeed");
    djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone())
        .create(djinn_db::CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task_id),
            model: "model-a",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("session create should succeed");
    task_id
}

async fn seed_running_session_with_task_run_in_project(
    app_state: &crate::host::SlotContext,
    project_id: &str,
    task_run_id: &str,
) -> String {
    let epic = test_helpers::create_test_epic(&app_state.db, project_id).await;
    let task = test_helpers::create_test_task(&app_state.db, project_id, &epic.id).await;
    let task_id = task.id.clone();
    djinn_db::TaskRunRepository::new(app_state.db.clone())
        .create(djinn_db::CreateTaskRunParams {
            id: task_run_id,
            project_id,
            task_id: &task_id,
            trigger_type: "test",
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("task_run create should succeed");
    djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone())
        .create(djinn_db::CreateSessionParams {
            project_id,
            task_id: Some(&task_id),
            model: "model-a",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("session create should succeed");
    task_id
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

#[tokio::test]
async fn slot_pool_metrics_aggregate_by_state_and_model_without_slot_labels() {
    djinn_telemetry::init().unwrap();
    let (app_state, cancel, _temp) = test_app_state();
    let config = make_config(
        vec![
            model("model-a", 2, &["worker"]),
            model("model-b", 1, &["worker"]),
        ],
        &[("worker", vec!["model-a", "model-b"])],
    );
    let (_tx, rx) = mpsc::channel(1);
    let mut pool = SlotPool::new(rx, app_state, cancel, config);
    pool.test_assign_busy("task-a", 0);
    pool.test_record_slot_pool_metrics();
    let rendered = djinn_telemetry::render().unwrap();
    assert!(rendered.contains("djinn_slot_pool"));
    assert!(rendered.contains("model=\"model-a\""));
    assert!(rendered.contains("model=\"model-b\""));
    assert!(rendered.contains("state=\"busy\""));
    assert!(rendered.contains("state=\"free\""));
    assert!(!rendered.contains("slot_id="));
    assert!(!rendered.contains("task-a"));
}

fn test_slot_factory(
    runtime: Duration,
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
) -> SlotFactory {
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let signal_tx = signal_tx.clone();
        let runner: super::super::actor::TestLifecycleRunner = Arc::new(
            move |task_id,
                  _project_path,
                  _model_id,
                  _app_state,
                  kill,
                  pause,
                  _resume_lifecycle_metadata| {
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

fn blocking_cancel_slot_factory(
    runtime: Duration,
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
    release_after_cancel: Arc<Notify>,
) -> SlotFactory {
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let signal_tx = signal_tx.clone();
        let release_after_cancel = release_after_cancel.clone();
        let runner: super::super::actor::TestLifecycleRunner = Arc::new(
            move |task_id,
                  _project_path,
                  _model_id,
                  _app_state,
                  kill,
                  pause,
                  _resume_lifecycle_metadata| {
                let signal_tx = signal_tx.clone();
                let release_after_cancel = release_after_cancel.clone();
                Box::pin(async move {
                    let _ = signal_tx.send(RunnerSignal::Started(task_id.clone()));
                    tokio::select! {
                        _ = tokio::time::sleep(runtime) => {
                            let _ = signal_tx.send(RunnerSignal::Completed(task_id));
                        }
                        _ = kill.cancelled() => {
                            let _ = signal_tx.send(RunnerSignal::Killed(task_id));
                            release_after_cancel.notified().await;
                        }
                        _ = pause.cancelled() => {
                            let _ = signal_tx.send(RunnerSignal::Paused(task_id));
                            release_after_cancel.notified().await;
                        }
                    }
                    Ok(())
                })
            },
        );
        SlotHandle::spawn_with_test_runner(slot_id, model_id, event_tx, app_state, cancel, runner)
    })
}

struct SlotPoolInvariantHarness<'a> {
    pool: &'a SlotPool,
}

impl<'a> SlotPoolInvariantHarness<'a> {
    fn new(pool: &'a SlotPool) -> Self {
        Self { pool }
    }
    fn assert_after(&self, event: &str) {
        self.assert_per_model_free_list_uniqueness(event);
        self.assert_retired_slots_absent_from_free_lists(event);
        self.assert_mapped_or_busy_slots_not_free(event);
    }
    fn assert_per_model_free_list_uniqueness(&self, event: &str) {
        for (model_id, free_slots) in self.pool.test_free_slots_by_model() {
            let mut seen = HashSet::new();
            for slot_id in &free_slots {
                assert!(
                    seen.insert(*slot_id),
                    "slot-pool invariant failed after {event}: model '{model_id}' free list contains duplicate slot id {slot_id}; free_slots={free_slots:?}"
                );
            }
        }
    }
    fn assert_retired_slots_absent_from_free_lists(&self, event: &str) {
        let retired = self.pool.test_retired_slots();
        for (model_id, free_slots) in self.pool.test_free_slots_by_model() {
            for slot_id in &free_slots {
                assert!(
                    !retired.contains(slot_id),
                    "slot-pool invariant failed after {event}: retired slot id {slot_id} is present in model '{model_id}' free list; retired_slots={retired:?}, free_slots={free_slots:?}"
                );
            }
        }
    }
    fn assert_mapped_or_busy_slots_not_free(&self, event: &str) {
        let task_slots = self.pool.test_task_slots();
        let slot_states = self.pool.test_slot_states();
        for (model_id, free_slots) in self.pool.test_free_slots_by_model() {
            for slot_id in &free_slots {
                if let Some((task_id, _)) = task_slots.iter().find(|(_, mapped)| *mapped == slot_id)
                {
                    panic!(
                        "slot-pool invariant failed after {event}: mapped slot id {slot_id} for task '{task_id}' is present in model '{model_id}' free list; free_slots={free_slots:?}, task_slots={task_slots:?}"
                    );
                }
                if let Some(SlotState::Busy { task_id, .. }) = slot_states.get(slot_id) {
                    panic!(
                        "slot-pool invariant failed after {event}: busy slot id {slot_id} for task '{task_id}' is present in model '{model_id}' free list; free_slots={free_slots:?}"
                    );
                }
            }
        }
    }
}

fn assert_slot_pool_invariants_after(pool: &SlotPool, event: &str) {
    SlotPoolInvariantHarness::new(pool).assert_after(event);
}

fn inject_stale_busy_free_slot(pool: &mut SlotPool, task_id: &str, model_id: &str) -> usize {
    let slot_id = pool.test_slot_of(task_id).unwrap_or_else(|| {
        panic!("task '{task_id}' should hold a slot before stale-free injection")
    });
    assert!(
        !pool.test_free_slots(model_id).contains(&slot_id),
        "test precondition failed: slot id {slot_id} for task '{task_id}' is already free on model '{model_id}' before stale-free injection"
    );
    pool.test_inject_free(slot_id, model_id);
    slot_id
}

fn new_white_box_pool(slot_count: u32) -> (SlotPool, TempDir) {
    let (app_state, cancel, temp) = test_app_state();
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", slot_count, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let (_pool_tx, pool_rx) = mpsc::channel(8);
    let pool = SlotPool::new_with_factory(
        pool_rx,
        app_state,
        cancel,
        config,
        // Long runtime: lifecycle ordering is driven only through the explicit
        // white-box hooks below, never by sleeps or natural completion.
        test_slot_factory(Duration::from_secs(3600), signal_tx),
    );
    (pool, temp)
}

#[tokio::test]
async fn snapshot_reports_free_busy_and_draining_slots() {
    let (mut pool, _temp) = new_white_box_pool(3);
    pool.test_set_slot_model(0, "model-a");
    pool.test_set_slot_model(1, "model-a");
    pool.test_set_slot_model(2, "model-b");
    pool.test_set_slot_state(0, SlotState::Free);
    pool.test_set_slot_state(
        1,
        SlotState::Busy {
            task_id: "task-busy".to_owned(),
            started_at: "12345".to_owned(),
            agent_type: "worker".to_owned(),
        },
    );
    pool.test_set_slot_state(2, SlotState::Draining);
    pool.test_set_task_slot("task-busy", 1);
    pool.test_set_task_slot("task-draining", 2);
    let snapshot = pool.snapshot();
    assert_eq!(snapshot.len(), 3);
    assert_eq!(snapshot[0].state, "free");
    assert_eq!(snapshot[0].model, "model-a");
    assert_eq!(snapshot[0].task_id, None);
    assert_eq!(snapshot[0].started_at, None);
    assert_eq!(snapshot[1].state, "busy");
    assert_eq!(snapshot[1].model, "model-a");
    assert_eq!(snapshot[1].task_id.as_deref(), Some("task-busy"));
    assert_eq!(snapshot[1].started_at.as_deref(), Some("12345"));
    assert_eq!(snapshot[2].state, "draining");
    assert_eq!(snapshot[2].model, "model-b");
    assert_eq!(snapshot[2].task_id.as_deref(), Some("task-draining"));
    assert_eq!(snapshot[2].started_at, None);
}

#[derive(Debug, Clone, Copy)]
enum LifecycleEventKind {
    Free,
    Killed,
}

impl LifecycleEventKind {
    fn event(self, slot_id: usize, task_id: &str) -> SlotEvent {
        match self {
            Self::Free => SlotEvent::Free {
                slot_id,
                model_id: "model-a".to_string(),
                task_id: task_id.to_string(),
            },
            Self::Killed => SlotEvent::Killed {
                slot_id,
                model_id: "model-a".to_string(),
                task_id: task_id.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LifecycleStep {
    Dispatch(&'static str),
    Terminate(&'static str),
    Complete(&'static str, LifecycleEventKind),
    DuplicateComplete(&'static str, LifecycleEventKind),
    MarkSlotFree(&'static str),
    RetireSlot(&'static str),
    DispatchAfterPoisonSelfHeals {
        poisoned_task: &'static str,
        next_task: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
struct LifecyclePermutation {
    name: &'static str,
    steps: &'static [LifecycleStep],
}

async fn run_lifecycle_permutation(case: LifecyclePermutation) {
    let (mut pool, _temp) = new_white_box_pool(1);
    let mut task_slots: HashMap<&'static str, usize> = HashMap::new();
    assert_slot_pool_invariants_after(&pool, &format!("{}: initial spawn", case.name));
    for (idx, step) in case.steps.iter().copied().enumerate() {
        let label = format!("{} step {idx} {step:?}", case.name);
        match step {
            LifecycleStep::Dispatch(task_id) => {
                pool.test_dispatch(task_id, "/tmp/project", "model-a")
                    .await
                    .unwrap_or_else(|err| panic!("{label}: dispatch {task_id} failed: {err:?}"));
                let slot_id = pool
                    .test_slot_of(task_id)
                    .unwrap_or_else(|| panic!("{label}: {task_id} should hold a slot"));
                task_slots.insert(task_id, slot_id);
                assert_slot_pool_invariants_after(&pool, &label);
            }
            LifecycleStep::Terminate(task_id) => {
                pool.test_terminate_session(task_id)
                    .await
                    .unwrap_or_else(|err| panic!("{label}: terminate {task_id} failed: {err:?}"));
                assert_slot_pool_invariants_after(&pool, &label);
            }
            LifecycleStep::Complete(task_id, kind)
            | LifecycleStep::DuplicateComplete(task_id, kind) => {
                let slot_id = *task_slots
                    .get(task_id)
                    .unwrap_or_else(|| panic!("{label}: no recorded slot for {task_id}"));
                pool.test_handle_slot_event(kind.event(slot_id, task_id))
                    .await;
                assert_slot_pool_invariants_after(&pool, &label);
            }
            LifecycleStep::MarkSlotFree(task_id) => {
                let slot_id = *task_slots
                    .get(task_id)
                    .unwrap_or_else(|| panic!("{label}: no recorded slot for {task_id}"));
                pool.test_mark_slot_free(slot_id, "model-a");
                assert_slot_pool_invariants_after(&pool, &label);
            }
            LifecycleStep::RetireSlot(task_id) => {
                let slot_id = *task_slots
                    .get(task_id)
                    .unwrap_or_else(|| panic!("{label}: no recorded slot for {task_id}"));
                pool.test_retire(slot_id);
                assert_slot_pool_invariants_after(&pool, &label);
            }
            LifecycleStep::DispatchAfterPoisonSelfHeals {
                poisoned_task,
                next_task,
            } => {
                let poisoned_slot =
                    inject_stale_busy_free_slot(&mut pool, poisoned_task, "model-a");
                assert!(
                    pool.test_free_slots("model-a").contains(&poisoned_slot),
                    "{label}: poisoned busy slot should be present on the free list before self-heal"
                );
                pool.test_dispatch(next_task, "/tmp/project", "model-a")
                    .await
                    .unwrap_or_else(|err| {
                        panic!("{label}: dispatch {next_task} after poison failed: {err:?}")
                    });
                assert_slot_pool_invariants_after(&pool, &label);
                let next_slot = pool
                    .test_slot_of(next_task)
                    .unwrap_or_else(|| panic!("{label}: {next_task} should hold a slot"));
                assert_ne!(
                    next_slot, poisoned_slot,
                    "{label}: dispatch must not hand out the still-busy poisoned slot"
                );
                assert!(
                    !pool.test_free_slots("model-a").contains(&poisoned_slot),
                    "{label}: poisoned busy slot must be dropped from the free list"
                );
                task_slots.insert(next_task, next_slot);
            }
        }
    }
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

async fn wait_for_signal(
    signal_rx: &mut mpsc::UnboundedReceiver<RunnerSignal>,
    description: &str,
    predicate: impl Fn(&RunnerSignal) -> bool,
) -> RunnerSignal {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for runner signal: {description}"
        );
        let signal = tokio::time::timeout(remaining, signal_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for runner signal: {description}"))
            .unwrap_or_else(|| {
                panic!("runner signal channel closed while waiting for {description}")
            });
        if predicate(&signal) {
            return signal;
        }
    }
}

async fn assert_actor_pool_status(
    pool: &SlotPoolHandle,
    label: &str,
    total: u32,
    active: u32,
    free: u32,
    running_tasks: usize,
) {
    let status = pool
        .get_status()
        .await
        .unwrap_or_else(|err| panic!("{label}: get_status failed: {err:?}"));
    let model_status = status
        .per_model
        .get("model-a")
        .unwrap_or_else(|| panic!("{label}: model-a should be present in status: {status:?}"));
    assert_eq!(status.total_slots, total as usize, "{label}: total slots");
    assert_eq!(
        status.active_slots, active as usize,
        "{label}: active slots"
    );
    assert_eq!(model_status.total, total, "{label}: model total");
    assert_eq!(model_status.active, active, "{label}: model active");
    assert_eq!(model_status.free, free, "{label}: model free");
    assert_eq!(
        status.running_tasks.len(),
        running_tasks,
        "{label}: running task mappings"
    );
    assert!(
        model_status.free <= model_status.total.saturating_sub(model_status.active),
        "{label}: get_status must not report duplicate free capacity: {model_status:?}"
    );
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
                blocked_by: None,
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
            pricing: None,
            cost_basis: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_session_tears_down_taskrun_job_and_ignores_teardown_errors() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(true);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_id = seed_running_session_with_task_run(&app_state, "kill teardown", "run-kill").await;
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
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    pool.kill_session(&task_id)
        .await
        .expect("teardown failure must not fail kill_session");
    assert!(
        runtime.calls().iter().any(|call| call == "run-kill"),
        "kill_session should attempt task-run Job teardown"
    );
    wait_until_no_sessions(&pool, &[task_id]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evict_session_tears_down_taskrun_job_before_reclaiming_slot() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_id =
        seed_running_session_with_task_run(&app_state, "evict teardown", "run-evict").await;
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
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    pool.evict_session(&task_id)
        .await
        .expect("evict_session should succeed");
    assert!(
        runtime.calls().iter().any(|call| call == "run-evict"),
        "evict_session should attempt task-run Job teardown"
    );
    assert!(
        !pool
            .has_session(&task_id)
            .await
            .expect("has_session should succeed"),
        "evict_session should still reclaim the task mapping"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_handle_evict_then_late_killed_event_preserves_reclaimed_mapping() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(true);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_id = seed_running_session_with_task_run(
        &app_state,
        "actor evict lifecycle race",
        "run-actor-evict-race",
    )
    .await;
    let release_after_cancel = Arc::new(Notify::new());
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 1, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        blocking_cancel_slot_factory(
            Duration::from_secs(3600),
            signal_tx,
            release_after_cancel.clone(),
        ),
    );
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("initial dispatch should succeed");
    wait_for_signal(
        &mut signal_rx,
        "initial task started",
        |signal| matches!(signal, RunnerSignal::Started(started) if started == &task_id),
    )
    .await;
    let original_slot = pool
        .session_for_task(&task_id)
        .await
        .expect("session lookup should succeed")
        .expect("initial task should have an active session")
        .slot_id;
    assert_actor_pool_status(&pool, "initial dispatch", 1, 1, 0, 1).await;
    pool.evict_session(&task_id)
        .await
        .expect("evict_session should reclaim the mapping even when teardown fails");
    wait_for_signal(
        &mut signal_rx,
        "evicted task observed kill",
        |signal| matches!(signal, RunnerSignal::Killed(killed) if killed == &task_id),
    )
    .await;
    assert!(
        runtime
            .calls()
            .iter()
            .any(|call| call == "run-actor-evict-race"),
        "evict_session must attempt task-run Job teardown before reclaim"
    );
    assert!(
        !pool
            .has_session(&task_id)
            .await
            .expect("has_session should succeed"),
        "evict_session must synchronously remove the stale task mapping"
    );
    assert!(
        pool.session_for_task(&task_id)
            .await
            .expect("session lookup should succeed")
            .is_none(),
        "evict_session must remove stale session_for_task state"
    );
    assert_actor_pool_status(&pool, "after evict before lifecycle Killed", 1, 1, 0, 0).await;
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("reclaimed task should redispatch before the old Killed event arrives");
    wait_for_signal(
        &mut signal_rx,
        "redispatched task started",
        |signal| matches!(signal, RunnerSignal::Started(started) if started == &task_id),
    )
    .await;
    let redispatched_slot = pool
        .session_for_task(&task_id)
        .await
        .expect("session lookup should succeed")
        .expect("redispatched task should have an active session")
        .slot_id;
    assert_ne!(
        redispatched_slot, original_slot,
        "redispatch before the old Killed event must not reuse the still-killing slot"
    );
    assert_actor_pool_status(&pool, "redispatch before lifecycle Killed", 2, 2, 0, 1).await;
    release_after_cancel.notify_waiters();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = pool.get_status().await.expect("status should succeed");
        let model_status = status
            .per_model
            .get("model-a")
            .expect("model-a should be present");
        if model_status.free == 1 && model_status.active == 1 && status.running_tasks.len() == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for late Killed event to free exactly one old slot; status={status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        pool.session_for_task(&task_id)
            .await
            .expect("session lookup should succeed")
            .expect("redispatched task mapping must survive stale Killed event")
            .slot_id,
        redispatched_slot,
        "late Killed event for the reclaimed slot must not remove the redispatched mapping"
    );
    assert_actor_pool_status(&pool, "after late Killed event", 2, 1, 1, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_all_tears_down_each_running_taskrun_job() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_a = seed_running_session_with_task_run(&app_state, "interrupt a", "run-int-a").await;
    let task_b = seed_running_session_with_task_run(&app_state, "interrupt b", "run-int-b").await;
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
    pool.dispatch(&task_a, "/tmp/project", "model-a")
        .await
        .expect("dispatch A should succeed");
    pool.dispatch(&task_b, "/tmp/project", "model-a")
        .await
        .expect("dispatch B should succeed");
    pool.interrupt_all("test interrupt")
        .await
        .expect("interrupt_all should succeed");
    let calls = runtime.calls();
    assert!(calls.iter().any(|call| call == "run-int-a"));
    assert!(calls.iter().any(|call| call == "run-int-b"));
    wait_until_no_sessions(&pool, &[task_a, task_b]).await;
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
    assert_slot_pool_invariants_after(&pool, "dispatch task-a");
    // Inject the exact desync: the still-busy slot back on the free list.
    let slot_a = inject_stale_busy_free_slot(&mut pool, "task-a", "model-a");
    // Must self-heal: drop the stale entry and spawn a fresh slot — NOT wedge.
    pool.test_dispatch("task-b", "/tmp/project", "model-a")
        .await
        .expect("second dispatch must recover instead of wedging on the busy slot");
    assert_slot_pool_invariants_after(&pool, "dispatch task-b after stale busy free-list entry");
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

/// Focused harness regression: the invariant helper guards the stale-busy-slot
/// self-heal path that later table-driven lifecycle race tests will exercise
/// after each event. The intentionally poisoned free-list entry is dropped on
/// dispatch, rather than requeued, and the reusable helper verifies uniqueness,
/// retired-slot exclusion, and busy/mapped-slot exclusion in one assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invariant_harness_accepts_stale_busy_slot_self_heal() {
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
    assert_slot_pool_invariants_after(&pool, "initial spawn");
    pool.test_dispatch("task-a", "/tmp/project", "model-a")
        .await
        .expect("first dispatch should occupy the pre-warmed slot");
    let stale_slot = inject_stale_busy_free_slot(&mut pool, "task-a", "model-a");
    pool.test_dispatch("task-b", "/tmp/project", "model-a")
        .await
        .expect("dispatch should self-heal the stale busy free-list entry");
    assert_slot_pool_invariants_after(&pool, "self-healed dispatch after stale busy injection");
    assert!(
        !pool.test_free_slots("model-a").contains(&stale_slot),
        "stale busy slot id {stale_slot} must be dropped instead of requeued"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_permutations_preserve_slot_pool_invariants() {
    const CASES: &[LifecyclePermutation] = &[
        LifecyclePermutation {
            name: "duplicate-free-completion-is-idempotent",
            steps: &[
                LifecycleStep::Dispatch("task-a"),
                LifecycleStep::Complete("task-a", LifecycleEventKind::Free),
                LifecycleStep::DuplicateComplete("task-a", LifecycleEventKind::Free),
                LifecycleStep::MarkSlotFree("task-a"),
            ],
        },
        LifecyclePermutation {
            name: "terminate-before-stale-free-completion",
            steps: &[
                LifecycleStep::Dispatch("task-a"),
                LifecycleStep::Terminate("task-a"),
                LifecycleStep::Complete("task-a", LifecycleEventKind::Free),
                LifecycleStep::DuplicateComplete("task-a", LifecycleEventKind::Killed),
            ],
        },
        LifecyclePermutation {
            name: "terminate-before-stale-killed-completion",
            steps: &[
                LifecycleStep::Dispatch("task-a"),
                LifecycleStep::Terminate("task-a"),
                LifecycleStep::Complete("task-a", LifecycleEventKind::Killed),
                LifecycleStep::DuplicateComplete("task-a", LifecycleEventKind::Killed),
            ],
        },
        LifecyclePermutation {
            name: "killed-completion-before-duplicate-free",
            steps: &[
                LifecycleStep::Dispatch("task-a"),
                LifecycleStep::Complete("task-a", LifecycleEventKind::Killed),
                LifecycleStep::DuplicateComplete("task-a", LifecycleEventKind::Free),
                LifecycleStep::MarkSlotFree("task-a"),
            ],
        },
        LifecyclePermutation {
            name: "retired-slot-ignores-late-completions",
            steps: &[
                LifecycleStep::Dispatch("task-a"),
                LifecycleStep::RetireSlot("task-a"),
                LifecycleStep::Complete("task-a", LifecycleEventKind::Killed),
                LifecycleStep::DuplicateComplete("task-a", LifecycleEventKind::Free),
                LifecycleStep::MarkSlotFree("task-a"),
                LifecycleStep::Dispatch("task-b"),
            ],
        },
        LifecyclePermutation {
            name: "v0-4-14-stale-busy-free-list-wedge-self-heals",
            steps: &[
                LifecycleStep::Dispatch("task-a"),
                LifecycleStep::DispatchAfterPoisonSelfHeals {
                    poisoned_task: "task-a",
                    next_task: "task-b",
                },
            ],
        },
    ];
    for case in CASES {
        run_lifecycle_permutation(*case).await;
    }
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
    assert_slot_pool_invariants_after(&pool, "initial spawn");
    // Re-freeing an already-free slot is a no-op, never a duplicate.
    pool.test_mark_slot_free(0, "model-a");
    assert_slot_pool_invariants_after(&pool, "idempotent mark_slot_free");
    assert_eq!(
        pool.test_free_slots("model-a"),
        vec![0],
        "mark_slot_free must not duplicate an already-free slot"
    );
    // Take slot 0 out of the free list (as a dispatch would), then retire it.
    pool.test_dispatch("task-a", "/tmp/project", "model-a")
        .await
        .expect("dispatch should occupy slot 0");
    assert_slot_pool_invariants_after(&pool, "dispatch before retire");
    assert_eq!(pool.test_slot_of("task-a"), Some(0));
    assert!(!pool.test_free_slots("model-a").contains(&0));
    pool.test_retire(0);
    assert_slot_pool_invariants_after(&pool, "manual retire while busy");
    // A stale Free event for a retired slot must not return it to rotation.
    pool.test_mark_slot_free(0, "model-a");
    assert_slot_pool_invariants_after(&pool, "stale free event for retired slot");
    assert!(
        !pool.test_free_slots("model-a").contains(&0),
        "mark_slot_free must refuse to resurrect a retired slot"
    );
}

/// `interrupt_project` is the bulk-interrupt surface scoped to a single
/// project (e.g. project delete / leadership handoff). It must hit the
/// same teardown path as `interrupt_all` for every mapped task in that
/// project — NOT just settle the session row, but also delete the
/// `djinn-taskrun-{task_run_id}` Job so the K8s pod is terminated
/// promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_project_tears_down_each_affected_taskrun_job() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let project = test_helpers::create_test_project(&app_state.db).await;
    let other_project = test_helpers::create_test_project(&app_state.db).await;
    let project_id = project.id.clone();
    let other_project_id = other_project.id.clone();
    let task_a =
        seed_running_session_with_task_run_in_project(&app_state, &project_id, "run-proj-a").await;
    let task_b =
        seed_running_session_with_task_run_in_project(&app_state, &project_id, "run-proj-b").await;
    let task_other =
        seed_running_session_with_task_run_in_project(&app_state, &other_project_id, "run-other")
            .await;
    // Sanity: two affected tasks share one project, while the third task lives
    // in a different project and must survive the scoped interrupt.
    assert_ne!(project_id, other_project_id);
    let (signal_tx, _signal_rx) = mpsc::unbounded_channel();
    let config = make_config(
        vec![model("model-a", 3, &["worker"])],
        &[("worker", vec!["model-a"])],
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel,
        config,
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );
    pool.dispatch(&task_a, "/tmp/project", "model-a")
        .await
        .expect("dispatch A should succeed");
    pool.dispatch(&task_b, "/tmp/project", "model-a")
        .await
        .expect("dispatch B should succeed");
    pool.dispatch(&task_other, "/tmp/other-project", "model-a")
        .await
        .expect("dispatch other should succeed");
    pool.interrupt_project(&project_id, "test project interrupt")
        .await
        .expect("interrupt_project should succeed");
    let calls = runtime.calls();
    assert!(
        calls.iter().any(|call| call == "run-proj-a"),
        "task a (project {project_id}) should be torn down"
    );
    assert!(
        calls.iter().any(|call| call == "run-proj-b"),
        "task b (project {project_id}) should be torn down"
    );
    assert!(
        !calls.iter().any(|call| call == "run-other"),
        "task other (project {other_project_id}) must NOT be torn down"
    );
    wait_until_no_sessions(&pool, &[task_a, task_b]).await;
    assert!(
        pool.has_session(&task_other)
            .await
            .expect("has_session should succeed"),
        "unrelated project task should still be running"
    );
    pool.interrupt_all("test cleanup")
        .await
        .expect("cleanup interrupt_all should succeed");
    wait_until_no_sessions(&pool, &[task_other]).await;
}

/// `evict_session` for a task whose session has no `task_run_id` (e.g. a
/// purely synthetic paused session, or a session seeded without an
/// attached K8s task-run) must NOT crash, NOT invoke teardown, and
/// still reclaim the task mapping. This guards the
/// `teardown_taskrun_jobs_for_task` filter that skips non-running rows
/// and trims empty task-run ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evict_session_with_no_task_run_id_is_idempotent() {
    use djinn_db::CreateSessionParams;
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let project = crate::test_helpers::create_test_project(&app_state.db).await;
    let epic = crate::test_helpers::create_test_epic(&app_state.db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&app_state.db, &project.id, &epic.id).await;
    let task_id = task.id.clone();
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task_id),
            model: "model-a",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("session create should succeed");
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
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    pool.evict_session(&task_id)
        .await
        .expect("evict_session should succeed");
    assert!(
        runtime.calls().is_empty(),
        "evict_session with no task_run_id must not invoke teardown"
    );
    assert!(
        !pool
            .has_session(&task_id)
            .await
            .expect("has_session should succeed"),
        "evict_session should still reclaim the task mapping"
    );
}

/// `kill_session` for an unknown task id returns `PoolError::TaskNotFound`
/// and does NOT crash, leak, or call teardown. Idempotent error path —
/// a redispatch loop that double-invokes `kill_session` for the same
/// already-killed task id must not be able to wedge the slot pool on
/// the second call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_session_with_no_slot_mapping_returns_task_not_found() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
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
    pool.kill_session("ghost-task")
        .await
        .expect_err("kill_session on an unmapped task should return TaskNotFound");
    assert!(
        runtime.calls().is_empty(),
        "kill_session on an unmapped task must not call teardown"
    );
}

/// `interrupt_all` is idempotent at the teardown level: invoking it on
/// an empty pool is a no-op (no calls, no panic). A double-invoke
/// (the "shutdown tick after a forced kill" race the supervisor runner
/// hits) does not double-tear-down — once a slot is no longer in
/// `task_to_slot`, the inner `kill_session` returns `TaskNotFound` and
/// the outer `interrupt_all` keeps going.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_all_is_idempotent_and_skips_already_removed_sessions() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_id =
        seed_running_session_with_task_run(&app_state, "double interrupt", "run-double").await;
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
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    // First interrupt: tears down `run-double` once, then drains via the
    // slot `Killed` event handler.
    pool.interrupt_all("first interrupt")
        .await
        .expect("first interrupt_all should succeed");
    wait_until_no_sessions(&pool, std::slice::from_ref(&task_id)).await;
    // Second interrupt on the now-empty pool is a no-op: zero calls,
    // zero panics.
    let calls_before = runtime.calls().len();
    pool.interrupt_all("second interrupt (idempotent)")
        .await
        .expect("second interrupt_all should succeed");
    let calls_after = runtime.calls().len();
    assert_eq!(
        calls_after, calls_before,
        "interrupt_all on an already-drained pool must not invoke teardown again"
    );
}

/// The `SlotEvent::Killed` path is the backstop for kill routes that bypass
/// `kill_session`/`evict_session`: when a slot lifecycle directly reports a
/// killed task, the pool must still delete the task-run Job before settling the
/// session row. Drive the actor method directly so this coverage is independent
/// of the public `kill_session` path, which intentionally performs an earlier
/// teardown and settlement to avoid duplicate Kubernetes deletes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slot_event_killed_tears_down_taskrun_job() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_id =
        seed_running_session_with_task_run(&app_state, "killed event teardown", "run-killed").await;
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
        test_slot_factory(Duration::from_secs(10), signal_tx),
    );
    pool.test_dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    let slot_id = pool
        .test_slot_of(&task_id)
        .expect("task should hold a slot");
    pool.test_handle_slot_event(super::super::SlotEvent::Killed {
        slot_id,
        model_id: "model-a".to_string(),
        task_id: task_id.clone(),
    })
    .await;
    assert!(
        runtime.calls().iter().any(|call| call == "run-killed"),
        "SlotEvent::Killed handler must tear down the task-run Job (saw calls: {:?})",
        runtime.calls()
    );
    let count = runtime
        .calls()
        .iter()
        .filter(|call| **call == "run-killed")
        .count();
    assert_eq!(
        count, 1,
        "SlotEvent::Killed must invoke teardown exactly once (saw {count} calls)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_session_synchronously_reclaims_mapping_activity_and_session_row() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
    let task_id = seed_running_session_with_task_run(
        &app_state,
        "terminate reclaim",
        "run-terminate-reclaim",
    )
    .await;
    let session_repo =
        djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    app_state.register_activity(&task_id);
    assert!(
        app_state.idle_seconds(&task_id).is_some(),
        "test should start with tracked activity"
    );
    let app_state_for_assert = app_state.clone();
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
    pool.dispatch(&task_id, "/tmp/project", "model-a")
        .await
        .expect("dispatch should succeed");
    assert_eq!(running_count_for_cap(&session_repo).await, 1);
    pool.terminate_session(&task_id)
        .await
        .expect("terminate_session should succeed");
    assert!(
        runtime
            .calls()
            .iter()
            .any(|call| call == "run-terminate-reclaim"),
        "terminate_session should attempt task-run Job teardown"
    );
    assert!(
        !pool
            .has_session(&task_id)
            .await
            .expect("has_session should succeed"),
        "terminate_session should synchronously remove the task mapping"
    );
    assert!(
        pool.session_for_task(&task_id)
            .await
            .expect("session lookup should succeed")
            .is_none(),
        "terminate_session should remove task_started/task_projects-backed session info"
    );
    assert!(
        app_state_for_assert.idle_seconds(&task_id).is_none(),
        "terminate_session should deregister host activity"
    );
    assert_eq!(
        running_count_for_cap(&session_repo).await,
        0,
        "terminate_session should settle the running row before returning"
    );
    assert!(
        session_repo
            .list_active()
            .await
            .expect("list_active should succeed")
            .is_empty(),
        "no running sessions should remain after terminate_session returns"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_session_with_no_slot_mapping_returns_task_not_found() {
    let (mut app_state, cancel, _temp) = test_app_state();
    let runtime = RecordingRuntimeOps::new(false);
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));
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
    let err = pool
        .terminate_session("ghost-task")
        .await
        .expect_err("terminate_session on an unmapped task should fail truthfully");
    assert!(
        matches!(err, PoolError::TaskNotFound { ref task_id } if task_id == "ghost-task"),
        "expected TaskNotFound for the requested task id, got {err:?}"
    );
    assert!(
        runtime.calls().is_empty(),
        "terminate_session on an unmapped task must not call teardown"
    );
    pool.evict_session("ghost-task")
        .await
        .expect("evict_session keeps leaked-session idempotent no-op semantics");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_session_does_not_return_non_draining_slot_to_free_list() {
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
    pool.test_dispatch("task-terminate", "/tmp/project", "model-a")
        .await
        .expect("dispatch should occupy slot 0");
    let slot_id = pool
        .test_slot_of("task-terminate")
        .expect("task should hold a slot");
    assert_eq!(slot_id, 0);
    assert!(
        pool.test_free_slots("model-a").is_empty(),
        "busy slot should not be on the free list before termination"
    );
    pool.test_terminate_session("task-terminate")
        .await
        .expect("terminate_session should reclaim the task mapping");
    assert_eq!(pool.test_slot_of("task-terminate"), None);
    assert!(
        pool.test_free_slots("model-a").is_empty(),
        "terminate_session must not synchronously append a non-draining slot to the free list"
    );
    pool.test_dispatch("task-terminate", "/tmp/project", "model-a")
        .await
        .expect("terminated task should be immediately redispatchable before Killed event");
    let redispatched_slot = pool
        .test_slot_of("task-terminate")
        .expect("redispatched task should hold a new slot mapping");
    assert_ne!(
        redispatched_slot, slot_id,
        "redispatch before Killed should allocate a different slot rather than reusing the still-killing one"
    );
    assert!(
        pool.test_free_slots("model-a").is_empty(),
        "redispatch must not reuse or duplicate the still-killing slot"
    );
    pool.test_handle_slot_event(super::super::SlotEvent::Killed {
        slot_id,
        model_id: "model-a".to_string(),
        task_id: "task-terminate".to_string(),
    })
    .await;
    assert_eq!(
        pool.test_free_slots("model-a"),
        vec![slot_id],
        "the later lifecycle event is the single authority that frees the slot"
    );
    assert_eq!(
        pool.test_slot_of("task-terminate"),
        Some(redispatched_slot),
        "stale Killed event from the terminated slot must not remove the redispatched task mapping"
    );
}

// ---------------------------------------------------------------------------
// Compaction-aware teardown deferral tests (task t9iy)
// ---------------------------------------------------------------------------

/// Slot factory that captures the `CompactionCriticalSection` from each slot
/// handle into a shared map so tests can externally enter the compaction guard
/// and simulate an active compaction window.
fn compaction_capturing_slot_factory(
    runtime: Duration,
    signal_tx: mpsc::UnboundedSender<RunnerSignal>,
    captured_cses: std::sync::Arc<
        Mutex<HashMap<usize, crate::reply_loop::compaction_guard::CompactionCriticalSection>>,
    >,
) -> SlotFactory {
    Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
        let signal_tx = signal_tx.clone();
        let runner: super::super::actor::TestLifecycleRunner = Arc::new(
            move |task_id,
                  _project_path,
                  _model_id,
                  _app_state,
                  kill,
                  pause,
                  _resume_lifecycle_metadata| {
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
        let handle = super::super::actor::SlotHandle::spawn_with_test_runner(
            slot_id, model_id, event_tx, app_state, cancel, runner,
        );
        captured_cses
            .lock()
            .unwrap()
            .insert(slot_id, handle.test_compaction_cs().clone());
        handle
    })
}

/// White-box: `kill_session` for a compacting slot defers settlement and
/// mapping removal.  The task remains in `pending_teardown_tasks` until the
/// `SlotEvent::Killed` arrives.
#[tokio::test]
async fn kill_session_during_compaction_defers_settlement_and_mapping_release() {
    let (app_state, cancel, _temp) = test_app_state();
    let cses: std::sync::Arc<
        Mutex<HashMap<usize, crate::reply_loop::compaction_guard::CompactionCriticalSection>>,
    > = std::sync::Arc::new(Mutex::new(HashMap::new()));
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
        compaction_capturing_slot_factory(Duration::from_secs(3600), signal_tx, cses.clone()),
    );
    // Simulate dispatch: mark slot 0 busy with a task.
    pool.test_set_task_slot("task-1", 0);
    pool.test_assign_busy("task-1", 0);
    // Enter the compaction guard on the slot's CS — simulates the reply loop
    // being mid-compaction.
    let cs = cses.lock().unwrap().get(&0).unwrap().clone();
    let _guard = cs.guard();
    assert!(
        pool.test_slot_is_compacting(0),
        "slot should report compacting while guard is held"
    );
    // kill_session should defer: the task stays mapped, no settlement.
    pool.test_kill_session("task-1")
        .await
        .expect("kill_session should succeed even during compaction");
    assert!(
        pool.test_pending_teardown_tasks().contains("task-1"),
        "task should be in pending_teardown_tasks after deferred kill"
    );
    assert_eq!(
        pool.test_slot_of("task-1"),
        Some(0),
        "task mapping must NOT be removed during deferred teardown"
    );
    // Release compaction and simulate the eventual Killed event.
    drop(_guard);
    assert!(
        !pool.test_slot_is_compacting(0),
        "slot should no longer be compacting after guard is dropped"
    );
    pool.test_handle_slot_event(SlotEvent::Killed {
        slot_id: 0,
        model_id: "model-a".to_string(),
        task_id: "task-1".to_string(),
    })
    .await;
    assert!(
        pool.test_pending_teardown_tasks().is_empty(),
        "pending_teardown_tasks should be cleared after Killed event"
    );
    assert_eq!(
        pool.test_slot_of("task-1"),
        None,
        "task mapping should be removed after Killed event settles"
    );
}

/// White-box: `kill_session` for a non-compacting slot settles eagerly
/// (backwards-compatible behaviour).
#[tokio::test]
async fn kill_session_without_compaction_settles_eagerly() {
    let (app_state, cancel, _temp) = test_app_state();
    let cses: std::sync::Arc<
        Mutex<HashMap<usize, crate::reply_loop::compaction_guard::CompactionCriticalSection>>,
    > = std::sync::Arc::new(Mutex::new(HashMap::new()));
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
        compaction_capturing_slot_factory(Duration::from_secs(3600), signal_tx, cses.clone()),
    );
    pool.test_set_task_slot("task-1", 0);
    pool.test_assign_busy("task-1", 0);
    // No compaction guard held — normal path.
    assert!(
        !pool.test_slot_is_compacting(0),
        "slot should not be compacting"
    );
    pool.test_kill_session("task-1")
        .await
        .expect("kill_session should succeed");
    // Eager path: pending_teardown_tasks should NOT contain the task.
    assert!(
        !pool.test_pending_teardown_tasks().contains("task-1"),
        "non-compacting kill should NOT defer to pending_teardown_tasks"
    );
    // Mapping is still present (removed only when Killed event arrives).
    assert_eq!(
        pool.test_slot_of("task-1"),
        Some(0),
        "task mapping stays until the Killed event"
    );
    // Simulate the Killed event — mapping is removed.
    pool.test_handle_slot_event(SlotEvent::Killed {
        slot_id: 0,
        model_id: "model-a".to_string(),
        task_id: "task-1".to_string(),
    })
    .await;
    assert_eq!(pool.test_slot_of("task-1"), None);
}

/// White-box: repeated `kill_session` during compaction is idempotent — no
/// double-settle, no leaked pending entries.
#[tokio::test]
async fn kill_session_during_compaction_is_idempotent() {
    let (app_state, cancel, _temp) = test_app_state();
    let cses: std::sync::Arc<
        Mutex<HashMap<usize, crate::reply_loop::compaction_guard::CompactionCriticalSection>>,
    > = std::sync::Arc::new(Mutex::new(HashMap::new()));
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
        compaction_capturing_slot_factory(Duration::from_secs(3600), signal_tx, cses.clone()),
    );
    pool.test_set_task_slot("task-1", 0);
    pool.test_assign_busy("task-1", 0);
    let cs = cses.lock().unwrap().get(&0).unwrap().clone();
    let _guard = cs.guard();
    // First kill — defers.
    pool.test_kill_session("task-1")
        .await
        .expect("first kill_session should succeed");
    assert_eq!(pool.test_pending_teardown_tasks().len(), 1);
    // Second kill — idempotent no-op.
    pool.test_kill_session("task-1")
        .await
        .expect("second kill_session should succeed (idempotent)");
    assert_eq!(
        pool.test_pending_teardown_tasks().len(),
        1,
        "repeated kill must not add duplicate pending entries"
    );
    // Mapping still present.
    assert_eq!(pool.test_slot_of("task-1"), Some(0));
    // Release compaction, emit Killed, verify clean settlement.
    drop(_guard);
    pool.test_handle_slot_event(SlotEvent::Killed {
        slot_id: 0,
        model_id: "model-a".to_string(),
        task_id: "task-1".to_string(),
    })
    .await;
    assert!(pool.test_pending_teardown_tasks().is_empty());
    assert_eq!(pool.test_slot_of("task-1"), None);
}

/// White-box: `terminate_session` (reclaim) during compaction defers
/// settlement and mapping removal.
#[tokio::test]
async fn reclaim_session_during_compaction_defers_settlement() {
    let (app_state, cancel, _temp) = test_app_state();
    let cses: std::sync::Arc<
        Mutex<HashMap<usize, crate::reply_loop::compaction_guard::CompactionCriticalSection>>,
    > = std::sync::Arc::new(Mutex::new(HashMap::new()));
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
        compaction_capturing_slot_factory(Duration::from_secs(3600), signal_tx, cses.clone()),
    );
    pool.test_set_task_slot("task-1", 0);
    pool.test_assign_busy("task-1", 0);
    let cs = cses.lock().unwrap().get(&0).unwrap().clone();
    let _guard = cs.guard();
    // terminate_session goes through reclaim_session.
    pool.test_terminate_session("task-1")
        .await
        .expect("terminate_session should succeed during compaction");
    assert!(
        pool.test_pending_teardown_tasks().contains("task-1"),
        "reclaim during compaction should defer"
    );
    assert_eq!(
        pool.test_slot_of("task-1"),
        Some(0),
        "mapping must NOT be removed during deferred reclaim"
    );
    // Release compaction and emit Killed.
    drop(_guard);
    pool.test_handle_slot_event(SlotEvent::Killed {
        slot_id: 0,
        model_id: "model-a".to_string(),
        task_id: "task-1".to_string(),
    })
    .await;
    assert!(pool.test_pending_teardown_tasks().is_empty());
    assert_eq!(pool.test_slot_of("task-1"), None);
}
