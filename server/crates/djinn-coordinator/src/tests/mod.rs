// djinn:allow-oversize — shared test infrastructure (actors, helpers, fixtures)
// pushed file past the byte threshold; split when touched substantively.

use std::path::Path;
use std::sync::Mutex;

use serde_json::json;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::consolidation;
use super::dispatch::DispatchOutcome;
use super::*;
use crate::roles::RoleRegistry;
use crate::test_helpers;
use djinn_db::EpicRepository;
use djinn_db::NoteRepository;
use djinn_db::TaskRepository;
use djinn_db::repositories::task_attempt::{
    CreateTaskAttemptParams, TaskAttemptRepository, TerminalTaskAttemptParams,
};
use djinn_db::{
    CreateSessionParams, CreateTaskRunParams, DispatchPauseRepository, DispatchPauseTarget,
    SessionRepository, TaskRunRepository,
};
use djinn_provider::catalog::health::HealthTracker;
use djinn_slot::{ModelSlotConfig, SlotHandle, SlotPoolConfig, SlotPoolHandle};

#[derive(Clone)]
struct RecordingRuntimeOps {
    calls: Arc<Mutex<Vec<String>>>,
    taskrun_jobs: Arc<Mutex<Vec<djinn_control_plane::bridge::TaskrunJobRef>>>,
    fail_teardown: bool,
}

#[derive(Clone, Default)]
struct RecordingGraphWarmer {
    triggered: Arc<Mutex<Vec<String>>>,
}

impl RecordingGraphWarmer {
    fn triggered(&self) -> Vec<String> {
        self.triggered.lock().expect("warmer calls mutex").clone()
    }
}

#[async_trait::async_trait]
impl djinn_runtime::GraphWarmerService for RecordingGraphWarmer {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn trigger(&self, project_id: &str) {
        self.triggered
            .lock()
            .expect("warmer calls mutex")
            .push(project_id.to_owned());
    }

    async fn await_fresh(
        &self,
        _: &str,
        _: std::time::Duration,
        _: std::time::Duration,
    ) -> Result<(), djinn_runtime::WarmerError> {
        Ok(())
    }
}

impl RecordingRuntimeOps {
    fn new(fail_teardown: bool) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            taskrun_jobs: Arc::new(Mutex::new(Vec::new())),
            fail_teardown,
        }
    }

    fn with_taskrun_jobs(self, jobs: Vec<djinn_control_plane::bridge::TaskrunJobRef>) -> Self {
        *self.taskrun_jobs.lock().expect("runtime jobs mutex") = jobs;
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("runtime calls mutex").clone()
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
            .expect("runtime calls mutex")
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
        Ok(self
            .taskrun_jobs
            .lock()
            .expect("runtime jobs mutex")
            .clone())
    }

    async fn cleanup_task_branches(&self, _: &str) {}
}

fn spawn_coordinator(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> CoordinatorHandle {
    let cancel = CancellationToken::new();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let sessions_dir = std::env::temp_dir().join(format!(
        "djinn-test-sessions-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = sessions_dir;
    let pool = SlotPoolHandle::spawn(
        ctx,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 2,
                roles: ["worker", "reviewer"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
    );
    let catalog = CatalogService::new();
    let health = HealthTracker::new();
    let background_work_tracker = BackgroundWorkTracker::default();
    let role_registry = Arc::new(RoleRegistry::new());
    CoordinatorHandle::spawn(CoordinatorDeps::new(
        tx.clone(),
        cancel,
        db.clone(),
        pool,
        catalog,
        health,
        role_registry,
        background_work_tracker,
        djinn_lsp::LspManager::new(),
    ))
}

const MAX_IMAGE_ID_LEN: usize = 36;
const MAX_IMAGE_TAG_LEN: usize = 512;

fn assert_fits_varchar(value: &str, column: &str, max_len: usize) {
    assert!(
        value.len() <= max_len,
        "{column} is varchar({max_len}); generated test value was {} bytes",
        value.len()
    );
}

fn test_image_id() -> String {
    let id = uuid::Uuid::now_v7().simple().to_string();
    assert_fits_varchar(&id, "images.id", MAX_IMAGE_ID_LEN);
    id
}

fn test_image_tag(image_id: &str) -> String {
    let tag = format!("test-image-{}", &image_id[..20]);
    assert_fits_varchar(&tag, "images.tag", MAX_IMAGE_TAG_LEN);
    tag
}

async fn make_epic(
    db: &Database,
    tx: broadcast::Sender<DjinnEventEnvelope>,
) -> djinn_core::models::Epic {
    let epic = djinn_core::auth_context::SESSION_USER_ID
        .scope(
            None,
            EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
                .create("Epic", "", "", "", "", None),
        )
        .await
        .unwrap();
    // Satisfy the coordinator's readiness gate: assign a ready catalog
    // image to the synthesized default project and seed graph freshness rows.
    // The dispatch gate resolves readiness from `selected_image_id`, not
    // the legacy per-project image columns.
    let image_repo = djinn_db::ImageRepository::new(db.clone());
    // `images.id` is varchar(36), so use an unprefixed UUID payload.
    // Prefixing the id overflows CI's Postgres schema; the compact form
    // leaves extra headroom while remaining globally unique for tests.
    let image_id = test_image_id();
    image_repo
        .create(&image_id, "Test image", None, r#"{"schema_version":1}"#)
        .await
        .unwrap();
    // Keep the synthetic tag compact too: these tests run against the
    // real Postgres schema, whose image identity fields are length-bound.
    let image_tag = test_image_tag(&image_id);
    image_repo
        .mark_ready(&image_id, &image_tag, Some("sha256:testhash"))
        .await
        .unwrap();
    image_repo
        .set_project_image(&epic.project_id, Some(&image_id))
        .await
        .unwrap();
    let cache_repo = djinn_db::RepoGraphCacheRepository::new(db.clone());
    let _ = cache_repo
        .upsert(djinn_db::RepoGraphCacheInsert {
            project_id: &epic.project_id,
            commit_sha: "test-commit",
            graph_blob: b"test-graph",
        })
        .await;
    djinn_db::ProjectWorkspaceGraphRepository::new(db.clone())
        .upsert(djinn_db::ProjectWorkspaceGraphUpsert {
            project_id: &epic.project_id,
            workspace_slug: "root",
            commit_sha: "test-commit",
            status: "ready",
        })
        .await
        .unwrap();
    epic
}

async fn create_task_with_note(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    title: &str,
) -> (djinn_core::models::Task, djinn_memory::Note) {
    let project = test_helpers::create_test_project(db).await;
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
    std::fs::create_dir_all(&project_path).unwrap();
    let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .create_for_project(
            &project.id,
            djinn_db::EpicCreateInput {
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
        .unwrap();
    let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let note = note_repo
        .create(&project.id, title, "body", "research", "[]")
        .await
        .unwrap();
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let task = task_repo
        .create(&epic.id, title, "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let memory_refs = serde_json::to_string(&vec![note.permalink.clone()]).unwrap();
    let task = task_repo
        .update_memory_refs(&task.id, &memory_refs)
        .await
        .unwrap();
    note_repo.set_confidence(&note.id, 0.5).await.unwrap();
    (task, note)
}

/// Poll the activity log until a coordinator-recorded outcome marker with
/// `kind` and `reopen_count` exists for `task_id`, or panic on timeout.
///
/// The coordinator applies outcome-confidence penalties asynchronously:
/// `set_status*` logs a `status_changed` activity (which broadcasts an
/// event), and the coordinator actor later fetches the task, applies the
/// Bayesian penalty, and records the marker.  Tests that used a fixed
/// `sleep` to wait for that side-effect flaked under load because 50-
/// 150ms is not a hard upper bound on scheduler + DB latency.  Polling
/// for the marker directly observes the coordinator's completed work.
async fn wait_for_outcome_marker(
    repo: &TaskRepository,
    task_id: &str,
    kind: &str,
    reopen_count: i64,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let markers = repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        let found = markers.iter().any(|entry| {
            serde_json::from_str::<serde_json::Value>(&entry.payload)
                .ok()
                .map(|payload| {
                    payload.get("kind").and_then(serde_json::Value::as_str) == Some(kind)
                        && payload
                            .get("reopen_count")
                            .and_then(serde_json::Value::as_i64)
                            == Some(reopen_count)
                })
                .unwrap_or(false)
        });
        if found {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for outcome marker kind={kind} reopen_count={reopen_count} on task {task_id}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Count coordinator-recorded outcome-confidence markers for `task_id`.
///
/// Used to assert idempotency as an integer invariant (marker count
/// unchanged across a duplicate no-op event) rather than as a float-
/// equality check on the derived `confidence` — the latter flakes under
/// timing jitter because it conflates "penalty not applied" with
/// "penalty not YET applied".
async fn outcome_marker_count(repo: &TaskRepository, task_id: &str) -> usize {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
        actor_role: Some("system".to_string()),
        project_id: None,
        from_time: None,
        to_time: None,
        limit: 100,
        offset: 0,
    })
    .await
    .unwrap()
    .len()
}

fn coordinator_actor_for_tests(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> CoordinatorActor {
    CoordinatorActor {
        receiver: mpsc::channel(1).1,
        events: tx.subscribe(),
        cancel: CancellationToken::new(),
        tick: tokio::time::interval(STUCK_INTERVAL),
        db: db.clone(),
        coordinator_incarnation_id: uuid::Uuid::now_v7().to_string(),
        boot_at: ::time::OffsetDateTime::now_utc(),
        events_tx: tx.clone(),
        pool: SlotPoolHandle::spawn_with_factory(
            test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
            CancellationToken::new(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 1,
                    roles: ["worker", "reviewer"]
                        .into_iter()
                        .map(ToOwned::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
            Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
                let runner: djinn_slot::TestLifecycleRunner = Arc::new(
                    |_task_id,
                     _project_path,
                     _model_id,
                     _app_state,
                     _kill,
                     _pause,
                     _resume_lifecycle_metadata| { Box::pin(async { Ok(()) }) },
                );
                SlotHandle::spawn_with_test_runner(
                    slot_id, model_id, event_tx, app_state, cancel, runner,
                )
            }),
        ),
        build_admission: None,
        catalog: CatalogService::new(),
        health: HealthTracker::new(),
        role_registry: Arc::new(RoleRegistry::new()),
        lsp: djinn_lsp::LspManager::new(),
        self_sender: mpsc::channel(1).0,
        status_tx: watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        })
        .0,
        dispatch_limit: 50,
        model_priorities: HashMap::new(),
        #[cfg(test)]
        test_use_live_credential_resolution: false,
        pr_errors: HashMap::new(),
        last_dispatched: HashMap::new(),
        inflight_dispatches: HashMap::new(),
        provisional_admissions: HashMap::new(),
        dispatch_cooldowns: HashMap::new(),
        dispatch_failure_streak: HashMap::new(),
        background_work_tracker: BackgroundWorkTracker::default(),
        auto_merge_tracker: AutoMergeTracker::default(),
        consolidation_runner: Arc::new(consolidation::DbConsolidationRunner::new(db.clone())),
        mismatch_scan: crate::doctor::mismatch_scan::MismatchScanCoordinator::new(
            db.clone(),
            crate::events::event_bus_for(tx),
        ),
        last_stale_sweep: StdInstant::now(),
        last_auto_dispatch_sweep: StdInstant::now(),
        last_proposal_review_sweep: StdInstant::now(),
        last_graph_refresh: StdInstant::now(),
        graph_warmer: None,
        mirror: None,
        runtime_ops: None,
        rpc_registry: None,
        prune_tick_counter: 0,
        throughput_events: HashMap::new(),
        pr_status_cache: HashMap::new(),
        pr_draft_first_seen: HashMap::new(),
        review_stuck_sha_first_seen: HashMap::new(),
        merge_fail_count: HashMap::new(),
        auto_approve_attempted: HashMap::new(),
        delegated_to_github: HashMap::new(),
        conversations_resolved: HashMap::new(),
        handled_dequeues: HashMap::new(),
        stall_killed: HashSet::new(),
        stall_progress_watermark: HashMap::new(),
        stall_cancel_streak: HashMap::new(),
        stall_extension_count: HashMap::new(),
        provider_failure_streak: HashMap::new(),
        last_idle_consolidation: None,
        idle_consolidation_cancel: None,
        idle_consolidation_handle: None,
        pr_cleanup_config: PrCleanupConfig::default(),
        worker_lifecycle_config: crate::WorkerLifecycleConfig::default(),
        active_refinements: HashMap::new(),
        refinement_sessions: HashMap::new(),
        stranded_ready_source: None,
        closed_parent_open_children_source: None,
        dispatched: 0,
        recovered: 0,
    }
}

fn dispatch_state_record(
    task_id: &str,
    cooldown_until: Option<String>,
    counters: (i64, i64),
) -> djinn_db::DispatchStateRecord {
    djinn_db::DispatchStateRecord {
        task_id: task_id.to_owned(),
        failure_streak: counters.0,
        cooldown_until,
        escalation_count: counters.1,
        last_dispatched_at: None,
        last_dispatched_role: None,
        inflight_creator_user_id: None,
        inflight_model_id: None,
        updated_at: "2026-06-12T00:00:00.000Z".to_owned(),
    }
}

fn rfc3339(dt: ::time::OffsetDateTime) -> String {
    dt.format(&::time::format_description::well_known::Rfc3339)
        .unwrap()
}

#[tokio::test]
async fn dispatch_state_snapshot_filters_clones_and_does_not_mutate() {
    let db = Database::open_in_memory().unwrap();
    let (tx, _) = broadcast::channel(16);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let now = StdInstant::now();
    actor.dispatch_cooldowns.insert(
        "task-active-1234".to_owned(),
        now + std::time::Duration::from_secs(60),
    );
    actor.dispatch_cooldowns.insert(
        "task-expired-1234".to_owned(),
        now - std::time::Duration::from_secs(1),
    );
    actor
        .dispatch_failure_streak
        .insert("task-active-1234".to_owned(), 2);
    actor
        .dispatch_failure_streak
        .insert("task-zero-1234".to_owned(), 0);
    actor.inflight_dispatches.insert(
        "task-inflight-1234".to_owned(),
        InflightDispatch {
            creator: Some("user-1".to_owned()),
            model: "model-a".to_owned(),
            lane: djinn_core::models::ModelLane::Implement,
        },
    );
    actor.last_dispatched.insert(
        "task-inflight-1234".to_owned(),
        DispatchMarker {
            instant: now - std::time::Duration::from_secs(5),
            role: "worker".to_owned(),
        },
    );

    let before_cooldowns = actor.dispatch_cooldowns.clone();
    let before_streaks = actor.dispatch_failure_streak.clone();
    let before_inflight = actor.inflight_dispatches.clone();
    let first = actor.dispatch_state_snapshot();
    let second = actor.dispatch_state_snapshot();

    assert_eq!(actor.dispatch_cooldowns, before_cooldowns);
    assert_eq!(actor.dispatch_failure_streak, before_streaks);
    assert_eq!(actor.inflight_dispatches, before_inflight);
    assert_eq!(first.cooldowns.len(), 1);
    assert_eq!(first.cooldowns[0].task_id, "task-active-1234");
    assert_eq!(first.cooldowns[0].scope, "task");
    assert_eq!(first.failure_streaks.len(), 1);
    assert_eq!(first.failure_streaks[0].streak, 2);
    assert_eq!(first.inflight_ledger.len(), 1);
    assert_eq!(first.inflight_ledger[0].creator.as_deref(), Some("user-1"));
    assert_eq!(first.inflight_ledger[0].model, "model-a");
    assert_eq!(first.cooldowns.len(), second.cooldowns.len());
    assert_eq!(first.failure_streaks, second.failure_streaks);
    assert_eq!(first.inflight_ledger.len(), second.inflight_ledger.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_handle_debug_dispatch_state_round_trips() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let handle = spawn_coordinator(&db, &tx);
    let snapshot = handle.debug_dispatch_state().await.unwrap();
    assert!(snapshot.cooldowns.is_empty());
    assert!(snapshot.failure_streaks.is_empty());
    assert!(snapshot.inflight_ledger.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rehydration_loads_active_cooldown_and_durable_counters() {
    let db = Database::open_in_memory().unwrap();
    let (tx, _) = broadcast::channel(16);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let wall_now = ::time::OffsetDateTime::parse(
        "2026-06-12T00:00:00Z",
        &::time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let instant_now = StdInstant::now();
    let active_deadline = wall_now + ::time::Duration::seconds(90);

    let mut record = dispatch_state_record("task-active", Some(rfc3339(active_deadline)), (3, 2));
    record.last_dispatched_at = Some(rfc3339(wall_now - ::time::Duration::seconds(30)));
    record.last_dispatched_role = Some("reviewer".to_owned());
    record.inflight_creator_user_id = Some("user-1".to_owned());
    record.inflight_model_id = Some("provider/model".to_owned());

    let summary = actor.apply_rehydrated_dispatch_state(vec![record], wall_now, instant_now);

    assert_eq!(summary.records, 1);
    assert_eq!(summary.cooldowns, 1);
    assert_eq!(summary.expired_cooldowns, 0);
    assert_eq!(actor.dispatch_failure_streak["task-active"], 3);
    assert_eq!(actor.last_dispatched["task-active"].role, "reviewer");
    assert_eq!(
        actor.inflight_dispatches["task-active"],
        InflightDispatch {
            creator: Some("user-1".to_owned()),
            model: "provider/model".to_owned(),
            lane: djinn_core::models::ModelLane::Review,
        }
    );
    let remaining = actor.dispatch_cooldowns["task-active"].duration_since(instant_now);
    assert!(remaining >= std::time::Duration::from_secs(89));
    assert!(remaining <= std::time::Duration::from_secs(91));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rehydration_skips_expired_cooldown_but_keeps_other_state() {
    let db = Database::open_in_memory().unwrap();
    let (tx, _) = broadcast::channel(16);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let wall_now = ::time::OffsetDateTime::parse(
        "2026-06-12T00:00:00Z",
        &::time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let expired_deadline = wall_now - ::time::Duration::seconds(1);

    let record = dispatch_state_record("task-expired", Some(rfc3339(expired_deadline)), (5, 0));
    let summary = actor.apply_rehydrated_dispatch_state(vec![record], wall_now, StdInstant::now());

    assert_eq!(summary.cooldowns, 0);
    assert_eq!(summary.expired_cooldowns, 1);
    assert!(!actor.dispatch_cooldowns.contains_key("task-expired"));
    assert_eq!(actor.dispatch_failure_streak["task-expired"], 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_rehydrated_active_cooldown_defers_until_persisted_wall_clock_deadline() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) = create_simple_task(&db, &tx, "task", "cooldown restart task").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo.set_status(&task.id, "open").await.unwrap();

    let wall_now = ::time::OffsetDateTime::now_utc();
    let persisted_deadline = wall_now + ::time::Duration::seconds(30);
    let record = dispatch_state_record(&task.id, Some(rfc3339(persisted_deadline)), (1, 0));

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.apply_rehydrated_dispatch_state(vec![record], wall_now, StdInstant::now());

    actor.dispatch_ready_tasks(None).await;
    assert_eq!(
        actor.dispatched, 0,
        "rehydrated active cooldown must suppress dispatch before the original wall-clock deadline"
    );
    assert!(!actor.last_dispatched.contains_key(&task.id));

    actor.dispatch_cooldowns.insert(
        task.id.clone(),
        StdInstant::now() - std::time::Duration::from_millis(1),
    );
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        actor.dispatched, 1,
        "same task should dispatch once the rehydrated persisted cooldown deadline has elapsed"
    );
    assert!(actor.last_dispatched.contains_key(&task.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_rehydrated_failure_streak_continues_to_trigger_b_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "trigger b restart task").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    let wall_now = ::time::OffsetDateTime::now_utc();
    let mut record = dispatch_state_record(
        &task.id,
        None,
        (i64::from(STREAK_INTERVENTION_THRESHOLD - 1), 0),
    );
    record.last_dispatched_at = Some(rfc3339(wall_now - ::time::Duration::seconds(5)));
    record.last_dispatched_role = Some("reviewer".to_owned());

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.apply_rehydrated_dispatch_state(vec![record], wall_now, StdInstant::now());
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        planner_intervention_markers(&repo, &task.id).await.len(),
        1,
        "failure streak N must survive restart so the next same-role failure reaches trigger B"
    );
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "trigger-B planner handoff clears the rehydrated backoff state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_rehydrated_failure_streak_continues_to_terminal_close_threshold() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) = create_simple_task(&db, &tx, "task", "terminal restart task").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();
    repo.log_activity(
        Some(&task.id),
        "coordinator",
        "system",
        PLANNER_INTERVENTION_MARKER,
        &serde_json::json!({"reopen_count": task.reopen_count}).to_string(),
    )
    .await
    .unwrap();

    let wall_now = ::time::OffsetDateTime::now_utc();
    let mut record =
        dispatch_state_record(&task.id, None, (i64::from(MAX_DISPATCH_FAILURES - 1), 0));
    record.last_dispatched_at = Some(rfc3339(wall_now - ::time::Duration::seconds(5)));
    record.last_dispatched_role = Some("reviewer".to_owned());

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.apply_rehydrated_dispatch_state(vec![record], wall_now, StdInstant::now());
    actor.dispatch_ready_tasks(None).await;

    let closed = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        closed.status, "closed",
        "failure streak N must survive restart so the next same-role failure reaches MAX_DISPATCH_FAILURES"
    );
    assert!(!actor.dispatch_failure_streak.contains_key(&task.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_cleanup_prunes_closed_terminal_dispatch_state_rows() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (closed_task, _project_path) = create_simple_task(&db, &tx, "task", "closed cleanup").await;
    let (open_task, _project_path) = create_simple_task(&db, &tx, "task", "open kept").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    repo.set_status(&closed_task.id, "closed").await.unwrap();

    let state_repo = djinn_db::DispatchStateRepository::new(db.clone());
    for task_id in [&closed_task.id, &open_task.id] {
        state_repo
            .upsert(djinn_db::DispatchStateUpsert {
                task_id,
                failure_streak: 2,
                cooldown_until: None,
                escalation_count: 1,
                last_dispatched_at: None,
                last_dispatched_role: None,
                inflight_creator_user_id: None,
                inflight_model_id: None,
            })
            .await
            .unwrap();
    }

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.rehydrate_durable_dispatch_state().await;

    assert!(state_repo.get(&closed_task.id).await.unwrap().is_none());
    assert!(state_repo.get(&open_task.id).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_pause_defers_periodic_graph_warm_triggers() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = test_helpers::create_test_project(&db).await;
    ProjectRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_environment_config(
            &project.id,
            r#"{"schema_version":1,"languages":{"rust":{}}}"#,
        )
        .await
        .unwrap();
    DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .pause(
            DispatchPauseTarget::global(),
            djinn_core::models::DispatchPause {
                paused_by: "admin".to_owned(),
                paused_at: "2026-06-12T00:00:00Z".to_owned(),
                reason: "maintenance".to_owned(),
                expires_at: None,
            },
        )
        .await
        .unwrap();

    let warmer = Arc::new(RecordingGraphWarmer::default());
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.graph_warmer = Some(warmer.clone());

    actor.refresh_canonical_graphs_if_stale().await;

    assert!(
        warmer.triggered().is_empty(),
        "global dispatch pause must skip periodic graph warm Job triggers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_rehydrates_persisted_inflight_ledger_for_cap_overlay() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let wall_now = ::time::OffsetDateTime::now_utc();
    let mut record = dispatch_state_record("task-inflight", None, (0, 0));
    record.last_dispatched_at = Some(rfc3339(wall_now - ::time::Duration::seconds(3)));
    record.last_dispatched_role = Some("worker".to_owned());
    record.inflight_creator_user_id = Some("user-a".to_owned());
    record.inflight_model_id = Some(DEFAULT_MODEL_ID.to_owned());

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.apply_rehydrated_dispatch_state(vec![record], wall_now, StdInstant::now());

    assert_eq!(
        actor.inflight_dispatches.get("task-inflight"),
        Some(&InflightDispatch {
            creator: Some("user-a".to_owned()),
            model: DEFAULT_MODEL_ID.to_owned(),
            lane: djinn_core::models::ModelLane::Implement,
        }),
        "chosen design: inflight ledger is persisted in dispatch_state and rehydrated at boot, so the existing cap overlay counts pre-running dispatches immediately after restart"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_mover_evidence_collects_dispatch_markers_pr_review_and_blockers() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) = create_simple_task(
        &db,
        &tx,
        "task",
        "live mover evidence dispatch and board facts",
    )
    .await;
    let (blocking_task, _project_path) =
        create_simple_task(&db, &tx, "task", "live mover blocker").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    repo.add_blocker(&task.id, &blocking_task.id).await.unwrap();
    let task = repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo
        .set_pr_url(&task.id, "https://github.com/example/repo/pull/1")
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "worker".to_owned(),
        },
    );
    actor.inflight_dispatches.insert(
        task.id.clone(),
        InflightDispatch {
            creator: None,
            model: DEFAULT_MODEL_ID.to_owned(),
            lane: djinn_core::models::ModelLane::Implement,
        },
    );

    let evidence = actor.collect_live_mover_evidence(&task).await.unwrap();

    assert!(!evidence.active_session);
    assert!(evidence.dispatch_inflight);
    assert!(evidence.recently_dispatched);
    assert!(evidence.open_pr);
    assert!(evidence.pr_poller_owned);
    assert!(evidence.unresolved_blockers);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_mover_evidence_collects_active_session() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, project_path) =
        create_simple_task(&db, &tx, "task", "live mover active session").await;
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = SlotPoolHandle::spawn_with_factory(
        test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
        CancellationToken::new(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 1,
                roles: ["worker", "reviewer"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 _kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async { std::future::pending::<anyhow::Result<()>>().await })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );

    actor
        .pool
        .dispatch(&task.id, &project_path, DEFAULT_MODEL_ID)
        .await
        .unwrap();

    let evidence = actor.collect_live_mover_evidence(&task).await.unwrap();

    assert!(evidence.active_session);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_mover_evidence_collects_no_evidence_for_closed_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "live mover no evidence").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo.set_status(&task.id, "closed").await.unwrap();
    let actor = coordinator_actor_for_tests(&db, &tx);

    let evidence = actor.collect_live_mover_evidence(&task).await.unwrap();

    assert_eq!(
        evidence,
        crate::supervisor_impl::disposition::LiveMoverEvidence::default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_mover_summary_handle_exposes_reusable_non_pr_api() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) = create_simple_task(
        &db,
        &tx,
        "task",
        "live mover handle API summarizes evidence",
    )
    .await;
    let (blocking_task, _project_path) =
        create_simple_task(&db, &tx, "task", "live mover handle blocker").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    repo.add_blocker(&task.id, &blocking_task.id).await.unwrap();
    let task = repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo
        .set_pr_url(&task.id, "https://github.com/example/repo/pull/2")
        .await
        .unwrap();
    let handle = spawn_coordinator(&db, &tx);

    let summary = handle.live_mover_summary(&task.id).await.unwrap();

    assert!(summary.is_live());
    assert_eq!(
        summary.reasons,
        vec![
            crate::supervisor_impl::LiveMoverReason::OpenPr,
            crate::supervisor_impl::LiveMoverReason::PrPollerOwned,
            crate::supervisor_impl::LiveMoverReason::UnresolvedBlockers,
        ],
        "the handle API must preserve the supervisor predicate reason order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_mover_summary_handle_reports_no_mover_for_closed_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "live mover handle no mover").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo.set_status(&task.id, "closed").await.unwrap();
    let handle = spawn_coordinator(&db, &tx);

    let summary = handle.live_mover_summary(&task.id).await.unwrap();

    assert!(!summary.is_live());
    assert!(summary.reasons.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collect_live_mover_evidence_for_bridge_round_trips_evidence() {
    // T4's bridge: collect_live_mover_evidence_for delegates to the
    // coordinator handle's live_mover_summary and reconstructs the
    // LiveMoverEvidence. This test asserts the bridge round-trips the
    // evidence struct: a task with an open PR + unresolved blockers produces
    // a LiveMoverEvidence whose open_pr and unresolved_blockers fields are
    // true.
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "bridge round-trips evidence").await;
    let (blocking_task, _project_path) =
        create_simple_task(&db, &tx, "task", "bridge round-trips blocker").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    repo.add_blocker(&task.id, &blocking_task.id).await.unwrap();
    let task = repo.set_status(&task.id, "pr_review").await.unwrap();
    let task = repo
        .set_pr_url(&task.id, "https://github.com/example/repo/pull/3")
        .await
        .unwrap();
    let handle = spawn_coordinator(&db, &tx);

    let evidence = crate::doctor::live_mover::collect_live_mover_evidence_for(&handle, &task.id)
        .await
        .unwrap();

    // The bridge must reconstruct the evidence fields that the coordinator
    // collected: open_pr + pr_poller_owned (pr_review + pr_url) +
    // unresolved_blockers.
    assert!(
        evidence.open_pr,
        "open_pr must be true for a task with a non-empty pr_url"
    );
    assert!(
        evidence.pr_poller_owned,
        "pr_poller_owned must be true for pr_review status with pr_url"
    );
    assert!(
        evidence.unresolved_blockers,
        "unresolved_blockers must be true when the task has an open blocker"
    );
}

async fn create_simple_task(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    issue_type: &str,
    title: &str,
) -> (djinn_core::models::Task, String) {
    let project = test_helpers::create_test_project(db).await;
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
    std::fs::create_dir_all(&project_path).unwrap();
    let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .create_for_project(
            &project.id,
            djinn_db::EpicCreateInput {
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
        .unwrap();
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            title,
            "test task description",
            "test task design",
            issue_type,
            2,
            "test-owner",
            Some("approved"),
            None,
        )
        .await
        .unwrap();
    (task, project_path.to_string_lossy().into_owned())
}

/// Initialize a minimal git repo at `path` with an initial commit on
/// `main`.  Used by the architect-spike integration test to give the
/// session worktree a real git2-openable repo whose `git status` reflects
/// the durable artifact the test writes.
async fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();

    let output = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git init failed: {:?}", output);

    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(path)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .await;

    tokio::fs::write(path.join("README.md"), "base\n")
        .await
        .unwrap();
    let output = Command::new("git")
        .args(["add", "README.md"])
        .current_dir(path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git add failed: {:?}", output);

    let output = Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git commit failed: {:?}", output);
}

/// Variant of `spawn_coordinator` that returns the verification tracker
/// so tests can register/deregister tasks to simulate background work.
fn spawn_coordinator_with_tracker(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> (CoordinatorHandle, BackgroundWorkTracker) {
    let cancel = CancellationToken::new();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let pool = SlotPoolHandle::spawn(
        ctx,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 2,
                roles: ["worker", "reviewer"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
    );
    let catalog = CatalogService::new();
    let health = HealthTracker::new();
    let background_work_tracker = BackgroundWorkTracker::default();
    let tracker_clone = background_work_tracker.clone();
    let handle = CoordinatorHandle::spawn(CoordinatorDeps::new(
        tx.clone(),
        cancel,
        db.clone(),
        pool,
        catalog,
        health,
        Arc::new(RoleRegistry::new()),
        background_work_tracker,
        djinn_lsp::LspManager::new(),
    ));
    (handle, tracker_clone)
}

// ── Planner intervention for stuck tasks (trigger A) ──────────────────────

/// Create an `open` worker-eligible task and drive its `reopen_count` to
/// `target` via closed→open cycles (each reopen increments the count).
async fn make_task_with_reopen_count(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    target: i64,
) -> djinn_core::models::Task {
    let project = test_helpers::create_test_project(db).await;
    let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .create_for_project(
            &project.id,
            djinn_db::EpicCreateInput {
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
        .unwrap();
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Stuck task",
            "implements handlers but never registers the service",
            "",
            "task",
            0,
            "",
            Some("open"),
            Some(r#"[{"title":"fixture acceptance criterion"}]"#),
        )
        .await
        .unwrap();
    for _ in 0..target {
        repo.set_status(&task.id, "closed").await.unwrap();
        repo.set_status(&task.id, "open").await.unwrap();
    }
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.reopen_count, target, "test fixture reopen_count");
    task
}

/// uv3p Part B test support: seed terminated post-intervention worker sessions,
/// one per model in `models`, so the attempted-remediation park gate sees the
/// remediation was tried (and, at `NON_ATTEMPT_PARK_THRESHOLD` distinct models,
/// parks). Each session is `failed` with an `ended_at` — a genuine
/// pre-submission termination with no `work_submitted` logged. A short real
/// sleep before creation guarantees `started_at` sorts strictly after the
/// `last_intervention_at`/`human_review_resolved_at` floor (millisecond-format
/// timestamps), matching the production ordering where a post-intervention
/// dispatch lands seconds after the intervention.
///
/// Since `post_intervention_history` now derives its state from `task_attempts`
/// rows (not sessions), each seeded session also gets a matching `task_attempts`
/// row: created as `pending`, then advanced to `crashed` (a pre-submission
/// terminal outcome) so the attempt row mirrors the session's lifecycle.
#[allow(dead_code)]
async fn seed_terminated_post_intervention_sessions(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    task_id: &str,
    models: &[&str],
) {
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let project_id = repo
        .get(task_id)
        .await
        .unwrap()
        .expect("seed task must exist")
        .project_id;
    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    for model in models {
        // Strictly after the intervention floor; the format truncates to
        // milliseconds so a few ms of real time guarantees a greater string.
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(task_id),
                model,
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        // Terminate it pre-submission (sets ended_at + a non-completed status);
        // no work_submitted activity is logged for it.
        session_repo
            .update(
                &session.id,
                djinn_core::models::SessionStatus::Failed,
                0,
                0,
                0,
                0,
                None,
            )
            .await
            .unwrap();

        // Mirror the session lifecycle in task_attempts so
        // post_intervention_history sees the pre-submission terminal attempt.
        // Use the full session.id (36-char UUID) as the attempt id and dispatch
        // key to avoid collisions when two sessions share a UUID v7 prefix.
        let attempt_id = session.id.clone();
        let dispatch_key = format!("seed-{}", session.id);
        let attempt = attempt_repo
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &attempt_id,
                task_id,
                role: "worker",
                dispatch_key: &dispatch_key,
                session_id: Some(&session.id),
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();
        // Use `Cancelled` (a non-infra pre-submission terminal) so the attempt
        // counts toward the non-attempt park escalation threshold. `Crashed`,
        // `TimedOut`, and `SpawnFailed` are now classified as infra (excluded
        // from `non_attempt_models`), so they must NOT be used here when the
        // test expects the park gate to fire on attempted-remediation evidence.
        attempt_repo
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt.id,
                outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Cancelled,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();
    }
}

/// uv3p Part B: the `park_attempted_remediation_redispatch` markers recorded
/// when the park rung declined to park and redispatched.
#[allow(dead_code)]
async fn park_redispatch_markers(repo: &TaskRepository, task_id: &str) -> Vec<serde_json::Value> {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(PARK_REDISPATCH_MARKER.to_string()),
        actor_role: Some("system".to_string()),
        project_id: None,
        from_time: None,
        to_time: None,
        limit: 100,
        offset: 0,
    })
    .await
    .unwrap()
    .into_iter()
    .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap())
    .collect()
}

async fn planner_intervention_markers(
    repo: &TaskRepository,
    task_id: &str,
) -> Vec<serde_json::Value> {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(PLANNER_INTERVENTION_MARKER.to_string()),
        actor_role: Some("system".to_string()),
        project_id: None,
        from_time: None,
        to_time: None,
        limit: 100,
        offset: 0,
    })
    .await
    .unwrap()
    .into_iter()
    .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap())
    .collect()
}

mod deploy_interruptions_environmental;
mod dispatch_flow;
mod doctor_proposal_spec_integrity_sweep_e2e;
mod doctor_stranded_ready_e2e;
mod intervention;
mod pause_is_not_fault;
mod proposal_spec_integrity_rollout_contract;
mod session_reaping;
mod status_and_stuck;
mod terminal_gate_latest_row;

// ─── Boundary checks: orchestration crate dependency invariants ──────────────
// Extracted to `boundary.rs` to stay under the server file-size guard.

mod boundary;
mod post_intervention_lane;

#[cfg(test)]
mod i3mv_regression_tests;

#[cfg(test)]
mod operator_explanation_tests;

#[cfg(test)]
mod raw_signal_bypass_guard;

#[cfg(test)]
mod terminal_gate_boundary;

#[cfg(test)]
mod tripwire_planner_escalation;

#[cfg(test)]
mod escalation_ceiling;

#[cfg(test)]
mod incarnation_lease_liveness;
