use std::path::Path;
use std::sync::Mutex;

use serde_json::json;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::consolidation;
use super::dispatch::DispatchOutcome;
use super::*;
use crate::actors::slot::{ModelSlotConfig, SlotHandle, SlotPoolConfig, SlotPoolHandle};
use crate::roles::RoleRegistry;
use crate::test_helpers;
use djinn_db::EpicRepository;
use djinn_db::NoteRepository;
use djinn_db::TaskRepository;
use djinn_db::{CreateSessionParams, SessionRepository};
use djinn_provider::catalog::health::HealthTracker;

#[derive(Clone)]
struct RecordingRuntimeOps {
    calls: Arc<Mutex<Vec<String>>>,
    taskrun_jobs: Arc<Mutex<Vec<djinn_control_plane::bridge::TaskrunJobRef>>>,
    fail_teardown: bool,
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

    async fn dispatch_verification_test(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn trigger_graph_warm(&self, _: &str) {}

    async fn provision_backing_service(
        &self,
        _: djinn_control_plane::bridge::ProvisionServiceRequest,
    ) -> Result<djinn_control_plane::bridge::ProvisionedService, String> {
        Err("not used".to_string())
    }

    async fn release_backing_service(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

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
    let verification_tracker = VerificationTracker::default();
    let role_registry = Arc::new(RoleRegistry::new());
    CoordinatorHandle::spawn(CoordinatorDeps::new(
        tx.clone(),
        cancel,
        db.clone(),
        pool,
        catalog,
        health,
        role_registry,
        verification_tracker,
        crate::lsp::LspManager::new(),
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
        receiver: tokio::sync::mpsc::channel(1).1,
        events: tx.subscribe(),
        cancel: CancellationToken::new(),
        tick: tokio::time::interval(STUCK_INTERVAL),
        db: db.clone(),
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
                let runner: crate::actors::slot::TestLifecycleRunner = Arc::new(
                    |_task_id, _project_path, _model_id, _app_state, _kill, _pause| {
                        Box::pin(async { Ok(()) })
                    },
                );
                SlotHandle::spawn_with_test_runner(
                    slot_id, model_id, event_tx, app_state, cancel, runner,
                )
            }),
        ),
        catalog: CatalogService::new(),
        health: HealthTracker::new(),
        role_registry: Arc::new(RoleRegistry::new()),
        lsp: crate::lsp::LspManager::new(),
        self_sender: tokio::sync::mpsc::channel(1).0,
        status_tx: tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        })
        .0,
        dispatch_limit: 50,
        model_priorities: HashMap::new(),
        pr_errors: HashMap::new(),
        last_dispatched: HashMap::new(),
        inflight_dispatches: HashMap::new(),
        dispatch_cooldowns: HashMap::new(),
        dispatch_failure_streak: HashMap::new(),
        verification_tracker: VerificationTracker::default(),
        auto_merge_tracker: AutoMergeTracker::default(),
        consolidation_runner: Arc::new(consolidation::DbConsolidationRunner::new(db.clone())),
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
        escalation_counts: HashMap::new(),
        pr_status_cache: HashMap::new(),
        pr_draft_first_seen: HashMap::new(),
        merge_fail_count: HashMap::new(),
        auto_approve_attempted: HashMap::new(),
        delegated_to_github: HashMap::new(),
        conversations_resolved: HashMap::new(),
        stall_killed: HashSet::new(),
        last_idle_consolidation: None,
        idle_consolidation_cancel: None,
        idle_consolidation_handle: None,
        dispatched: 0,
        recovered: 0,
    }
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
            },
        )
        .await
        .unwrap();
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .create_in_project(
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
) -> (CoordinatorHandle, VerificationTracker) {
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
    let verification_tracker = VerificationTracker::default();
    let tracker_clone = verification_tracker.clone();
    let handle = CoordinatorHandle::spawn(CoordinatorDeps::new(
        tx.clone(),
        cancel,
        db.clone(),
        pool,
        catalog,
        health,
        Arc::new(RoleRegistry::new()),
        verification_tracker,
        crate::lsp::LspManager::new(),
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
            },
        )
        .await
        .unwrap();
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Stuck task",
            "implements handlers but never registers the service",
            "",
            "task",
            0,
            "",
            Some("open"),
            None,
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

mod dispatch_flow;
mod intervention;
mod session_reaping;
mod status_and_stuck;
