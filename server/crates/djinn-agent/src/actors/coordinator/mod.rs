// CoordinatorActor — 1x global, orchestrates phase execution and task dispatch.
//
// Ryhl hand-rolled actor pattern (AGENT-01):
//   - `CoordinatorHandle` (mpsc sender) is the public API.
//   - `CoordinatorActor` (mpsc receiver) runs in a dedicated tokio task.
//
// Main loop (AGENT-07): tokio::select! over four arms:
//   1. CancellationToken — graceful shutdown.
//   2. mpsc message channel — API calls from MCP tools.
//   3. broadcast::Receiver<DjinnEventEnvelope> — react to open-task events.
//   4. 30-second Interval tick — stuck detection safety net (AGENT-08).
//
// These imports are used by child submodules (dispatch, health, wave, rules,
// pr_poller, prompt_eval) which use `use super::*;` to access the coordinator's
// shared vocabulary.  In non-test builds some may appear unused at _this_ level.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use tokio::sync::broadcast;

use crate::actors::slot::{PoolError, SlotPoolHandle};
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::GitSettingsRepository;
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
use djinn_git::GitActorHandle;
// These additional imports are only used by `#[cfg(test)]` blocks in child
// submodules (rules, health, prompt_eval, etc.) via `use super::*;`.
#[cfg(test)]
use djinn_db::Database;
#[cfg(test)]
use djinn_provider::catalog::CatalogService;

// ─── Submodules ──────────────────────────────────────────────────────────────

mod actor;
mod consolidation;
mod dispatch;
mod handle;
mod health;
mod messages;
pub(crate) mod pr_poller;
mod prompt_eval;
mod reentrance;
pub(crate) mod rules;
mod types;
mod wave;

// Re-export public types so the external API is unchanged.
pub use handle::CoordinatorHandle;
pub use types::{CoordinatorDeps, CoordinatorError, CoordinatorStatus, VerificationTracker};

// Re-export internal types for sibling submodules that use `use super::*;`.
use actor::CoordinatorActor;
use messages::CoordinatorMessage;
use types::*;

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;
    use tokio::process::Command;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::consolidation;
    use super::*;
    use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use djinn_core::models::TransitionAction;
    use djinn_db::EpicRepository;
    use djinn_db::NoteRepository;
    use djinn_db::TaskRepository;
    use djinn_db::{CreateSessionParams, SessionRepository};
    use djinn_db::{
        DoltHistoryMaintenanceAction, DoltHistoryMaintenanceExecution,
        DoltHistoryMaintenancePolicy, DoltHistoryMaintenanceService,
    };
    use djinn_provider::catalog::health::HealthTracker;

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

    async fn make_epic(
        db: &Database,
        tx: broadcast::Sender<DjinnEventEnvelope>,
    ) -> djinn_core::models::Epic {
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap()
    }

    async fn create_task_with_note(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        title: &str,
    ) -> (djinn_core::models::Task, djinn_core::models::Note) {
        let project = test_helpers::create_test_project(db).await;
        std::fs::create_dir_all(Path::new(&project.path)).unwrap();
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
            .create(
                &project.id,
                Path::new(&project.path),
                title,
                "body",
                "research",
                "[]",
            )
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
        sqlx::query("UPDATE notes SET confidence = 0.5 WHERE id = ?")
            .bind(&note.id)
            .execute(db.pool())
            .await
            .unwrap();
        (task, note)
    }

    #[tokio::test]
    async fn dolt_history_maintenance_defaults_to_safe_planning_only_cutover() {
        let service_db = Database::open_in_memory().unwrap();
        let service = DoltHistoryMaintenanceService::new(&service_db);
        // Post sqlite→Dolt migration: `open_in_memory` now returns a Dolt-
        // backed DB, so the service always reports dolt. The default policy
        // still has execution disabled (planning-only), which is what the
        // "safe cutover" invariant below asserts.
        assert!(service.is_dolt_backend());

        let policy = DoltHistoryMaintenancePolicy::default();
        assert!(!policy.execution_enabled);

        let plan = djinn_db::plan_dolt_history_maintenance(
            &policy,
            &djinn_db::DoltHistoryMaintenanceSnapshot {
                commit_count: 5_500,
                current_hour_utc: 3,
                non_main_branches: vec!["task/qhnb".to_string()],
                row_counts: vec![djinn_db::DoltHistoryTableCount {
                    table: "notes".to_string(),
                    row_count: 42,
                }],
            },
        );

        assert_eq!(plan.action, DoltHistoryMaintenanceAction::Flatten);
        assert!(!plan.is_safe_to_execute());
        let execution = if plan.action == DoltHistoryMaintenanceAction::None {
            DoltHistoryMaintenanceExecution::NoActionRequired
        } else if !plan.is_safe_to_execute() {
            DoltHistoryMaintenanceExecution::BlockedBySafetyChecks
        } else {
            DoltHistoryMaintenanceExecution::PlannedOnly
        };
        assert_eq!(
            execution,
            DoltHistoryMaintenanceExecution::BlockedBySafetyChecks
        );
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
            pool: SlotPoolHandle::spawn(
                test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
                CancellationToken::new(),
                SlotPoolConfig {
                    models: vec![ModelSlotConfig {
                        model_id: DEFAULT_MODEL_ID.to_owned(),
                        max_slots: 1,
                        roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                    }],
                    role_priorities: HashMap::new(),
                },
            ),
            catalog: CatalogService::new(),
            health: HealthTracker::new(),
            role_registry: Arc::new(RoleRegistry::new()),
            lsp: crate::lsp::LspManager::new(),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx: tokio::sync::watch::channel(SharedCoordinatorState {
                paused_projects: HashSet::new(),
                unhealthy_project_ids: HashSet::new(),
                unhealthy_project_errors: HashMap::new(),
                dispatched: 0,
                recovered: 0,
                epic_throughput: HashMap::new(),
                pr_errors: HashMap::new(),
                rate_limited_until: None,
            })
            .0,
            paused_projects: HashSet::new(),
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            unhealthy_projects: HashMap::new(),
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            verification_tracker: VerificationTracker::default(),
            consolidation_runner: Arc::new(consolidation::DbConsolidationRunner::new(db.clone())),
            last_stale_sweep: StdInstant::now(),
            last_auto_dispatch_sweep: StdInstant::now(),
            last_graph_refresh: StdInstant::now(),
            canonical_graph_warmer: None,
            prune_tick_counter: 0,
            last_patrol_completed: StdInstant::now(),
            next_patrol_interval: rules::DEFAULT_PLANNER_PATROL_INTERVAL,
            throughput_events: HashMap::new(),
            escalation_counts: HashMap::new(),
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
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
        std::fs::create_dir_all(Path::new(&project.path)).unwrap();
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
        (task, project.path)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approved_simple_task_without_durable_artifacts_closes_directly() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, _project_path) =
            create_simple_task(&db, &tx, "spike", "artifact-free spike").await;

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.process_approved_tasks().await;

        let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get(&task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "closed");
        assert_eq!(updated.close_reason.as_deref(), Some("completed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approved_simple_task_with_memory_write_signal_skips_direct_close() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, _project_path) =
            create_simple_task(&db, &tx, "research", "memory-writing research").await;

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "architect",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        session_repo
            .set_event_taxonomy(
                &session.id,
                &json!({"files_changed": 0, "notes_written": 1}).to_string(),
            )
            .await
            .unwrap();

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.process_approved_tasks().await;

        let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get(&task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "approved");
        assert_ne!(
            updated.close_reason.as_deref(),
            Some("simple-lifecycle task — no PR needed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approved_simple_task_with_djinn_comment_signal_skips_direct_close() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, _project_path) =
            create_simple_task(&db, &tx, "review", "commented review").await;

        TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .log_activity(
                Some(&task.id),
                "architect",
                "architect",
                "comment",
                &json!({"body": "Wrote ADR at .djinn/decisions/proposed/adr-123.md"}).to_string(),
            )
            .await
            .unwrap();

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.process_approved_tasks().await;

        let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get(&task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "approved");
        assert_ne!(
            updated.close_reason.as_deref(),
            Some("simple-lifecycle task — no PR needed")
        );
    }

    // ── Unit coverage for the real worktree git-status signal ─────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_has_uncommitted_changes_detects_untracked_file() {
        let tmp = test_helpers::test_tempdir("coordinator-worktree-status-");
        init_git_repo(tmp.path()).await;

        // Clean repo: no signal.
        assert!(!CoordinatorActor::worktree_has_uncommitted_changes(
            tmp.path()
        ));

        // Untracked file (the kind a `call_shell` mkdir/echo would leave).
        std::fs::create_dir_all(tmp.path().join(".djinn/decisions/proposed")).unwrap();
        std::fs::write(
            tmp.path().join(".djinn/decisions/proposed/adr-999.md"),
            "# new ADR\n",
        )
        .unwrap();

        assert!(
            CoordinatorActor::worktree_has_uncommitted_changes(tmp.path()),
            "untracked .djinn/decisions/proposed/adr-999.md must be detected"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_has_uncommitted_changes_detects_modified_tracked_file() {
        let tmp = test_helpers::test_tempdir("coordinator-worktree-status-");
        init_git_repo(tmp.path()).await;

        std::fs::write(tmp.path().join("README.md"), "base modified\n").unwrap();
        assert!(CoordinatorActor::worktree_has_uncommitted_changes(
            tmp.path()
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_has_uncommitted_changes_returns_false_for_missing_path() {
        let missing = std::path::PathBuf::from("/nonexistent/djinn/worktree/path/xyz");
        assert!(!CoordinatorActor::worktree_has_uncommitted_changes(
            &missing
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worktree_has_uncommitted_changes_returns_false_for_non_git_dir() {
        let tmp = test_helpers::test_tempdir("coordinator-worktree-status-");
        std::fs::write(tmp.path().join("loose-file.md"), "x").unwrap();
        assert!(!CoordinatorActor::worktree_has_uncommitted_changes(
            tmp.path()
        ));
    }

    // ── Integration coverage for the architect-spike scenario ─────────────────

    /// End-to-end regression for the dtn6 root cause: an architect-style spike
    /// session that produces an unstaged ADR file inside its worktree must
    /// NOT be auto-closed with `simple-lifecycle task — no PR needed`.
    ///
    /// This test deliberately:
    ///   - sets up a *real* git repo at the session worktree path,
    ///   - creates a *real* `sessions` row pointing at that worktree,
    ///   - writes a *real* untracked `.djinn/decisions/proposed/adr-*.md` file,
    ///   - injects NO synthetic event_taxonomy (the worktree-status signal
    ///     must be the one that triggers the routing change), and
    ///   - does NOT pre-create the `task/<short_id>` branch (the whole point
    ///     of the assertion is that we *route through* the PR flow because
    ///     the artifact was detected, instead of short-circuiting to close).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn architect_spike_with_real_adr_file_routes_through_pr_flow_via_worktree_signal() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, project_path) =
            create_simple_task(&db, &tx, "spike", "architect ADR spike").await;

        // Real worktree directory inside the project, initialized as a git repo
        // so git2 status() actually has something to read.
        let worktree_path = Path::new(&project_path)
            .join(".djinn")
            .join("worktrees")
            .join(&task.short_id);
        init_git_repo(&worktree_path).await;

        // The architect "writes the ADR" via a shell command — i.e. exactly
        // the kind of change session_extraction.rs would miss because it only
        // counts write/edit/apply_patch tool calls, not call_shell side
        // effects.  We model that here by creating the file directly with std::fs.
        std::fs::create_dir_all(worktree_path.join(".djinn/decisions/proposed")).unwrap();
        std::fs::write(
            worktree_path.join(".djinn/decisions/proposed/adr-dtn6-test.md"),
            "# ADR: dtn6 regression coverage\n\nbody body body\n",
        )
        .unwrap();

        // Real session row, with worktree_path set so the coordinator can find
        // it.  Note: NO event_taxonomy is set — we want to prove the
        // worktree-status signal is what actually fires.
        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "architect",
                worktree_path: Some(worktree_path.to_str().unwrap()),
                metadata_json: None,
            })
            .await
            .unwrap();
        session_repo.pause(&session.id, 0, 0).await.unwrap();

        // Pre-flight: verify the helper sees the change directly.  This rules
        // out test-environment quirks (e.g. git2 unable to open the repo)
        // before we make the higher-level routing assertion.
        assert!(
            CoordinatorActor::worktree_has_uncommitted_changes(&worktree_path),
            "test fixture broken: worktree should report uncommitted changes"
        );

        let actor = coordinator_actor_for_tests(&db, &tx);
        // Drive the same predicate process_approved_tasks() consults — this
        // exercises the real extraction path (DB query for worktree_path +
        // git2 status), no synthetic taxonomy injection.
        let durable = actor
            .simple_lifecycle_task_has_durable_artifacts(&task.id)
            .await;
        assert!(
            durable,
            "spike with real ADR file in worktree must be classified as durable"
        );

        // Now drive the full routing path.  Because the artifact is detected,
        // process_approved_tasks must NOT take the simple-lifecycle close
        // shortcut.  Without a pre-created task branch the merge attempt
        // itself will fail, but that failure is intentional: it leaves the
        // task in `approved` (via the SKIP_SENTINEL release action) instead
        // of closing it as `simple-lifecycle task — no PR needed`.
        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.process_approved_tasks().await;

        let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get(&task.id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            updated.close_reason.as_deref(),
            Some("simple-lifecycle task — no PR needed"),
            "task with durable ADR artifact must not auto-close as simple-lifecycle"
        );
        assert_ne!(
            updated.status, "closed",
            "task with durable ADR artifact must not be closed by the short-circuit"
        );
    }

    // ── Status ───────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_status_is_active() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let status = handle.get_status().unwrap();
        assert!(
            !status.paused,
            "coordinator should start active (no global pause state)"
        );
        assert_eq!(status.tasks_dispatched, 0);
        assert_eq!(status.sessions_recovered, 0);
    }

    // ── Pause / Resume ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_project_and_resume_toggle_state() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let project_id = "test-project-id";

        // Pausing a project marks it paused in project-scoped status.
        handle.pause_project(project_id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if handle.get_project_status(project_id).unwrap().paused {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for paused project status");
        assert!(handle.get_project_status(project_id).unwrap().paused);

        // Resuming removes it from the paused set.
        handle.resume_project(project_id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if !handle.get_project_status(project_id).unwrap().paused {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for resumed project status");
        assert!(!handle.get_project_status(project_id).unwrap().paused);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_while_project_paused_is_a_noop() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        repo.create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.pause_project(&epic.project_id).await.unwrap();
        handle
            .trigger_dispatch_for_project(&epic.project_id)
            .await
            .unwrap();
        // Give the actor a moment to process; dispatched count stays 0.
        tokio::task::yield_now().await;
        assert_eq!(handle.get_status().unwrap().tasks_dispatched, 0);
    }

    // ── Dispatch on open-task event ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_ready_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Create a ready task (open, no blockers).
        repo.create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.tasks_dispatched >= 1,
            "should have dispatched the ready task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_review_tasks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        let task = repo
            .create(&epic.id, "Review me", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        repo.update(
            &task.id,
            "Review me",
            "",
            "",
            0,
            "",
            "",
            r#"[{"description":"default","met":false}]"#,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::Start,
            "test",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
        repo.transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "test",
            "system",
            None,
            None,
        )
        .await
        .unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        // Dispatch; wait for it to complete.
        handle.wait_for_status(|s| s.tasks_dispatched >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.tasks_dispatched >= 1,
            "should dispatch task waiting for review"
        );
    }

    // ── Stuck detection ───────────────────────────────────────────────────────

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_detection_skips_task_with_background_post_session_work() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Create a task and manually put it in in_task_review (simulating a
        // reviewer session that just ended — slot freed, but background merge
        // is still running).
        let task = repo
            .create(&epic.id, "Reviewing", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        repo.set_status(&task.id, "in_task_review").await.unwrap();

        let (handle, tracker) = spawn_coordinator_with_tracker(&db, &tx);

        // Register the task in the verification tracker (same as
        // spawn_post_session_work does for real sessions).
        tracker.lock().unwrap().insert(task.id.clone());

        // Trigger stuck scan — task should NOT be recovered because it has
        // registered background work.
        handle.trigger_stuck_scan().await.unwrap();
        // Give the actor time to process.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let updated = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status, "in_task_review",
            "task with background work should NOT be recovered"
        );

        // Now deregister — simulating background work completing.
        tracker.lock().unwrap().remove(&task.id);

        // Trigger stuck scan again — this time the task should be recovered.
        handle.trigger_stuck_scan().await.unwrap();
        handle.wait_for_status(|s| s.sessions_recovered >= 1).await;

        let final_task = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            final_task.status, "needs_task_review",
            "task without background work should be recovered to needs_task_review"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_detection_releases_orphaned_in_progress_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let epic = make_epic(&db, tx.clone()).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Manually put a task in_progress (simulating an orphaned session).
        let task = repo
            .create(&epic.id, "Stuck", "", "", "task", 0, "", Some("open"))
            .await
            .unwrap();
        repo.set_status(&task.id, "in_progress").await.unwrap();

        let handle = spawn_coordinator(&db, &tx);
        handle.trigger_dispatch().await.unwrap();
        // Trigger dispatch to also run stuck detection; wait for recovery.
        handle.wait_for_status(|s| s.sessions_recovered >= 1).await;

        let status = handle.get_status().unwrap();
        assert!(
            status.sessions_recovered >= 1,
            "stuck task should have been recovered"
        );

        // The released task should now be back to open.
        let updated = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status, "open",
            "released task should be back to open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_closed_task_applies_failure_confidence_once() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let _handle = spawn_coordinator(&db, &tx);
        let (task, note) = create_task_with_note(&db, &tx, "failed-close").await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let note_after = note_repo.get(&note.id).await.unwrap().unwrap();
        assert!(note_after.confidence < 0.5);

        let markers = repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 20,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(markers.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&markers[0].payload).unwrap();
        assert_eq!(payload["kind"], TASK_OUTCOME_FAILED_CLOSE);
        assert_eq!(payload["reopen_count"], 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopened_twice_applies_failure_once_per_reopen_count() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let _handle = spawn_coordinator(&db, &tx);
        let (task, note) = create_task_with_note(&db, &tx, "reopen-twice").await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.set_status(&task.id, "open").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let reopened_once = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reopened_once.reopen_count, 1);
        let after_first = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!(after_first < 0.5, "first reopen should reduce confidence");

        repo.set_status(&task.id, "open").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let after_duplicate = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!((after_duplicate - after_first).abs() < 1e-9);

        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        repo.set_status(&task.id, "open").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let reopened_twice = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reopened_twice.reopen_count, 2);
        let after_second = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!(
            after_second <= after_first,
            "second reopen should not increase confidence, got after_second={after_second}, after_first={after_first}"
        );

        let markers = repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 20,
                offset: 0,
            })
            .await
            .unwrap();
        let reopen_markers: Vec<serde_json::Value> = markers
            .into_iter()
            .map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).unwrap())
            .filter(|payload: &serde_json::Value| payload["kind"] == TASK_OUTCOME_REOPEN_COUNT)
            .collect();
        assert_eq!(reopen_markers.len(), 2);
        assert!(
            reopen_markers
                .iter()
                .any(|payload| payload["reopen_count"] == 1)
        );
        assert!(
            reopen_markers
                .iter()
                .any(|payload| payload["reopen_count"] == 2)
        );
    }
}
