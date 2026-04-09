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
    use std::future::Future;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Mutex;

    use serde_json::json;
    use tokio::process::Command;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use djinn_core::models::TransitionAction;
    use djinn_db::EpicRepository;
    use djinn_db::NoteConsolidationRepository;
    use djinn_db::NoteRepository;
    use djinn_db::TaskRepository;
    use djinn_db::{CreateSessionParams, SessionRepository};
    use djinn_provider::catalog::health::HealthTracker;

    use super::consolidation::{self, ConsolidationRunner, DbConsolidationRunner};
    use djinn_provider::rate_limit::{activate_suppression_window, clear_suppression_window};

    struct RecordingConsolidationRunner {
        calls: Arc<Mutex<Vec<djinn_db::DbNoteGroup>>>,
        session_calls: Arc<Mutex<Vec<(djinn_db::DbNoteGroup, String)>>>,
    }

    impl RecordingConsolidationRunner {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                session_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        #[allow(dead_code)]
        fn groups(&self) -> Vec<djinn_db::DbNoteGroup> {
            self.calls.lock().unwrap().clone()
        }

        fn session_groups(&self) -> Vec<(djinn_db::DbNoteGroup, String)> {
            self.session_calls.lock().unwrap().clone()
        }
    }

    impl ConsolidationRunner for RecordingConsolidationRunner {
        fn run_for_group<'a>(
            &'a self,
            group: djinn_db::DbNoteGroup,
        ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(group);
                Ok(())
            })
        }

        fn run_for_group_in_session<'a>(
            &'a self,
            group: djinn_db::DbNoteGroup,
            session_id: String,
        ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.session_calls.lock().unwrap().push((group, session_id));
                Ok(())
            })
        }
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
        sqlx::query("UPDATE notes SET confidence = 0.5 WHERE id = ?1")
            .bind(&note.id)
            .execute(db.pool())
            .await
            .unwrap();
        (task, note)
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
            consolidation_runner: Arc::new(RecordingConsolidationRunner::new()),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hourly_background_tick_invokes_consolidation_runner_for_db_note_group() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        let note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm A",
                "Retry storm causes duplicate work during incident recovery.",
                "case",
                "[]",
            )
            .await
            .unwrap();
        let note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm B",
                "Retry storm causes duplicate work during incident recovery.",
                "case",
                "[]",
            )
            .await
            .unwrap();

        // Link both notes to the same session so session-scoped consolidation
        // discovers them.
        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_a.id, &session.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_b.id, &session.id)
            .await
            .unwrap();

        let runner = Arc::new(RecordingConsolidationRunner::new());
        let actor = CoordinatorActor {
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
            consolidation_runner: runner.clone(),
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
        };
        consolidation::run_note_consolidation(&actor.db, &actor.consolidation_runner).await;

        let session_groups = runner.session_groups();
        assert_eq!(session_groups.len(), 1);
        assert_eq!(session_groups[0].0.project_id, project.id);
        assert_eq!(session_groups[0].0.note_type, "case");
        assert_eq!(session_groups[0].1, session.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_consolidation_skips_during_rate_limit_and_resumes_after_clear() {
        clear_suppression_window();

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let runner = Arc::new(RecordingConsolidationRunner::new());

        let mut actor = CoordinatorActor {
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
            consolidation_runner: runner.clone(),
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
        };

        activate_suppression_window(std::time::Duration::from_secs(30));
        assert!(actor.should_skip_background_llm_work("idle_note_consolidation"));
        assert!(actor.current_rate_limited_until().is_some());
        assert!(actor.idle_consolidation_handle.is_none());

        clear_suppression_window();
        assert!(!actor.should_skip_background_llm_work("idle_note_consolidation"));
        actor.maybe_start_idle_consolidation().await;
        assert!(actor.idle_consolidation_handle.is_some());
        actor.cancel_idle_consolidation();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn below_threshold_clusters_are_noop_for_consolidation_runner() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        note_repo
            .create_db_note(
                &project.id,
                "Incident Pattern A",
                "Repeated timeout while syncing cache data.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        note_repo
            .create_db_note(
                &project.id,
                "Incident Pattern B",
                "Repeated timeout while syncing cache data.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let metrics_before = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert!(metrics_before.is_empty());

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group(djinn_db::DbNoteGroup {
                project_id: project.id.clone(),
                note_type: "pattern".to_string(),
                note_count: 2,
            })
            .await
            .unwrap();

        let metrics_after = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert!(
            metrics_after.is_empty(),
            "below-threshold groups should remain a no-op with no run bookkeeping"
        );

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(notes.len(), 2, "runner should not synthesize new notes");

        for note in &notes {
            let provenance = consolidation_repo.list_provenance(&note.id).await.unwrap();
            assert!(
                provenance.is_empty(),
                "below-threshold groups should not persist provenance for note {}",
                note.id
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn qualifying_clusters_create_canonical_note_provenance_and_completed_metric() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());

        let note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm A",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm B",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let note_c = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm C",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind("Retry storms amplify duplicate work during recovery.")
            .bind("Prefer backoff and idempotent recovery steps.")
            .bind(&note_a.id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind("Retry storms amplify duplicate work during recovery.")
            .bind("Throttle retries before cache warmup completes.")
            .bind(&note_b.id)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
            .bind("Retry storms amplify duplicate work during recovery.")
            .bind("Use idempotent jobs plus exponential backoff.")
            .bind(&note_c.id)
            .execute(db.pool())
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session_a = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let session_b = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let session_c = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_a.id, &session_a.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_b.id, &session_b.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_c.id, &session_c.id)
            .await
            .unwrap();

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group(djinn_db::DbNoteGroup {
                project_id: project.id.clone(),
                note_type: "pattern".to_string(),
                note_count: 3,
            })
            .await
            .unwrap();

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(
            notes.len(),
            4,
            "runner should synthesize exactly one canonical note"
        );
        let canonical = notes
            .iter()
            .find(|note| note.id != note_a.id && note.id != note_b.id && note.id != note_c.id)
            .unwrap();
        assert!(
            canonical
                .title
                .starts_with("Canonical pattern: Retry Storm")
        );
        assert!(canonical.content.contains("## Source notes"));
        assert!(canonical.content.contains(&note_a.permalink));
        assert_eq!(
            canonical.abstract_.as_deref(),
            Some("Retry storms amplify duplicate work during recovery.")
        );
        assert!(canonical.confidence >= 0.65 && canonical.confidence <= 0.8);

        let provenance = consolidation_repo
            .list_provenance(&canonical.id)
            .await
            .unwrap();
        assert_eq!(
            provenance
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                session_a.id.as_str(),
                session_b.id.as_str(),
                session_c.id.as_str()
            ]
        );

        let metrics = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        let metric = &metrics[0];
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 3);
        assert_eq!(metric.candidate_cluster_count, 1);
        assert_eq!(metric.consolidated_cluster_count, 1);
        assert_eq!(metric.consolidated_note_count, 1);
        assert_eq!(metric.source_note_count, 3);
        assert!(metric.completed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_scoped_consolidation_excludes_cross_session_notes_and_preserves_metrics() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());

        let session_note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster A",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let session_note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster B",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let session_note_c = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster C",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let cross_session_note = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster D",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        for (note_id, overview) in [
            (
                &session_note_a.id,
                "Prefer backoff and idempotent recovery steps.",
            ),
            (
                &session_note_b.id,
                "Throttle retries before cache warmup completes.",
            ),
            (
                &session_note_c.id,
                "Use idempotent jobs plus exponential backoff.",
            ),
            (
                &cross_session_note.id,
                "A later session found the same retry pattern independently.",
            ),
        ] {
            sqlx::query("UPDATE notes SET abstract = ?1, overview = ?2 WHERE id = ?3")
                .bind("Retry storms amplify duplicate work during recovery.")
                .bind(overview)
                .bind(note_id)
                .execute(db.pool())
                .await
                .unwrap();
        }

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let source_session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        let later_session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .unwrap();

        for note_id in [&session_note_a.id, &session_note_b.id, &session_note_c.id] {
            consolidation_repo
                .add_provenance(note_id, &source_session.id)
                .await
                .unwrap();
        }
        consolidation_repo
            .add_provenance(&cross_session_note.id, &later_session.id)
            .await
            .unwrap();

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group_in_session(
                djinn_db::DbNoteGroup {
                    project_id: project.id.clone(),
                    note_type: "pattern".to_string(),
                    note_count: 3,
                },
                source_session.id.clone(),
            )
            .await
            .unwrap();

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(
            notes.len(),
            5,
            "session-scoped run should create one canonical note"
        );
        let canonical = notes
            .iter()
            .find(|note| {
                ![
                    &session_note_a.id,
                    &session_note_b.id,
                    &session_note_c.id,
                    &cross_session_note.id,
                ]
                .contains(&&note.id)
            })
            .unwrap();
        assert!(canonical.content.contains(&session_note_a.permalink));
        assert!(canonical.content.contains(&session_note_b.permalink));
        assert!(canonical.content.contains(&session_note_c.permalink));
        assert!(
            !canonical.content.contains(&cross_session_note.permalink),
            "canonical content must exclude unrelated cross-session note"
        );

        let provenance = consolidation_repo
            .list_provenance(&canonical.id)
            .await
            .unwrap();
        assert_eq!(
            provenance
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![source_session.id.as_str()],
            "session-scoped canonical note should inherit only same-session provenance"
        );

        let metrics = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        let metric = &metrics[0];
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 3);
        assert_eq!(metric.candidate_cluster_count, 1);
        assert_eq!(metric.consolidated_cluster_count, 1);
        assert_eq!(metric.consolidated_note_count, 1);
        assert_eq!(metric.source_note_count, 3);
        assert!(metric.completed_at.is_some());
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

    // ── Planner patrol dispatch (per ADR-051 §1) ──────────────────────────────

    // ── Wave-based Planner decomposition (task watx) ──────────────────────────

    /// Spawn a coordinator that includes "planner" and "architect" model slots,
    /// used by both patrol tests (planner-owned per ADR-051 §1) and wave
    /// decomposition tests.
    fn spawn_coordinator_with_planner(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
    ) -> CoordinatorHandle {
        let cancel = CancellationToken::new();
        let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = SlotPoolHandle::spawn(
            ctx,
            cancel.clone(),
            SlotPoolConfig {
                models: vec![ModelSlotConfig {
                    model_id: DEFAULT_MODEL_ID.to_owned(),
                    max_slots: 4,
                    roles: ["worker", "reviewer", "planner", "architect"]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_skips_when_no_open_epics() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // No epics at all — patrol should skip without creating any tasks.
        let handle = spawn_coordinator_with_planner(&db, &tx);
        handle.trigger_planner_patrol().await.unwrap();
        // Give actor time to process.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let review_tasks = task_repo
            .list_ready(djinn_db::ReadyQuery {
                issue_type: Some("review".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(
            review_tasks.is_empty(),
            "patrol should not create review task when there are no open epics"
        );
        // Dispatch counter should remain 0.
        assert_eq!(
            handle.get_status().unwrap().tasks_dispatched,
            0,
            "patrol should not dispatch when no open epics"
        );
    }

    /// Helper: poll until there is at least `min_count` decomposition tasks for
    /// the given epic (open or in-progress), or timeout after 2 seconds.
    async fn wait_for_decomp_tasks(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        epic_id: &str,
        min_count: usize,
    ) -> Vec<djinn_core::models::Task> {
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let tasks = task_repo.list_by_epic(epic_id).await.unwrap_or_default();
            let open_decomp: Vec<_> = tasks
                .into_iter()
                .filter(|t| {
                    matches!(t.issue_type.as_str(), "planning" | "decomposition")
                        && matches!(t.status.as_str(), "open" | "in_progress")
                })
                .collect();
            if open_decomp.len() >= min_count {
                return open_decomp;
            }
            if tokio::time::Instant::now() >= deadline {
                return open_decomp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_creation_triggers_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // Create project and epic BEFORE spawning coordinator so the project
        // is not auto-paused (coordinator pauses projects only when it receives
        // a project_created event — but create_test_project uses noop bus).
        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        // Spawn coordinator BEFORE creating epic so it receives the event.
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        // Yield to give the coordinator task a chance to start.
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Wave Test Epic",
                    description: "test",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        // Wait for the coordinator to process the epic_created event and create
        // the decomposition task (polling with 2s timeout).
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;

        assert_eq!(
            decomp_tasks.len(),
            1,
            "expected 1 decomposition task after epic creation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_skips_when_planner_already_running() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // Create an open epic so the patrol would normally run.
        let project = test_helpers::create_test_project(&db).await;
        EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Test Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        // Insert a fake running Planner session into the DB to simulate one already running.
        // Per ADR-051 §1 the Planner owns patrol; any active Planner session suppresses a
        // new patrol dispatch.
        let session_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status, started_at)
             VALUES (?1, ?2, NULL, 'test/mock', 'planner', 'running', strftime('%s','now'))",
        )
        .bind(&session_id)
        .bind(&project.id)
        .execute(db.pool())
        .await
        .unwrap();

        let handle = spawn_coordinator_with_planner(&db, &tx);
        handle.trigger_planner_patrol().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Dispatch counter should remain 0 — patrol was skipped.
        assert_eq!(
            handle.get_status().unwrap().tasks_dispatched,
            0,
            "patrol should skip when a Planner session is already running"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_creation_does_not_create_duplicate_decomposition_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Dedup Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        // Wait for the first decomposition task to be created.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1, "expected 1 decomposition task");

        // Send a duplicate epic_created event (e.g. from a sync artifact).
        let _ = tx.send(DjinnEventEnvelope::epic_created(&epic));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning_count = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(
            open_planning_count, 1,
            "duplicate epic_created events should not create duplicate planning tasks"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_epic_creation_does_not_trigger_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a drafting epic (the new default).
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Drafting Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        // Give coordinator time to process.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(
            planning_count, 0,
            "drafting epic should not trigger planning task creation"
        );
    }

    /// ADR-051 Epic C — proposed epics (architect-drafted shells) must
    /// never trigger auto-dispatch until they are explicitly accepted and
    /// promoted to `open`.  This is the live-coordinator safety rule
    /// spelled out in the ADR's Epic C section.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposed_epic_creation_does_not_trigger_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Proposed Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("proposed"),
                    auto_breakdown: None,
                    originating_adr_id: Some("adr-999-test"),
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(
            planning_count, 0,
            "proposed epic must never trigger planning task creation — \
             live-coordinator safety rule per ADR-051 Epic C"
        );
        assert_eq!(epic.status, "proposed");
        assert_eq!(epic.originating_adr_id.as_deref(), Some("adr-999-test"));
    }

    /// ADR-051 Epic C — when an epic is created with `auto_breakdown=false`,
    /// the coordinator must not dispatch a breakdown Planner even if the
    /// epic is open.  Used by `propose_adr_accept` to create epic shells
    /// without auto-dispatching.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_epic_with_auto_breakdown_false_skips_dispatch() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "No Auto Breakdown Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let planning_count = tasks
            .iter()
            .filter(|t| matches!(t.issue_type.as_str(), "planning" | "decomposition"))
            .count();
        assert_eq!(
            planning_count, 0,
            "epic with auto_breakdown=false must not trigger auto-dispatch"
        );
        assert!(!epic.auto_breakdown);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_to_open_promotion_triggers_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a drafting epic — should NOT trigger planning.
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Promote Me Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Promote: update status to open directly and fire epic_updated event.
        sqlx::query("UPDATE epics SET status = 'open' WHERE id = ?1")
            .bind(&epic.id)
            .execute(db.pool())
            .await
            .unwrap();
        let promoted: djinn_core::models::Epic =
            sqlx::query_as("SELECT id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, closed_at, created_at, updated_at, auto_breakdown, originating_adr_id FROM epics WHERE id = ?1")
                .bind(&epic.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));

        // Wait for planning task creation.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            decomp_tasks.len(),
            1,
            "drafting→open promotion should create exactly one planning task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drafting_to_open_promotion_does_not_duplicate_planning_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        // Create a drafting epic and promote it.
        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "No Dup Promote Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("drafting"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Promote to open.
        sqlx::query("UPDATE epics SET status = 'open' WHERE id = ?1")
            .bind(&epic.id)
            .execute(db.pool())
            .await
            .unwrap();
        let promoted: djinn_core::models::Epic =
            sqlx::query_as("SELECT id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, closed_at, created_at, updated_at, auto_breakdown, originating_adr_id FROM epics WHERE id = ?1")
                .bind(&epic.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));

        // Wait for first planning task.
        let decomp_tasks = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(decomp_tasks.len(), 1);

        // Send another epic_updated event (e.g. title change while still open).
        let _ = tx.send(DjinnEventEnvelope::epic_updated(&promoted));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks = task_repo.list_by_epic(&epic.id).await.unwrap();
        let open_planning_count = tasks
            .iter()
            .filter(|t| {
                matches!(t.issue_type.as_str(), "planning" | "decomposition")
                    && matches!(t.status.as_str(), "open" | "in_progress")
            })
            .count();
        assert_eq!(
            open_planning_count, 1,
            "repeated epic_updated events should not duplicate planning tasks"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn patrol_creates_review_task_when_open_epic_exists() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        // Create project with an open epic and an open task.
        let project = test_helpers::create_test_project(&db).await;
        let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Active Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        // Add an open task so the empty-board precondition passes.
        TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .create_in_project(
                &project.id,
                Some(&epic.id),
                "Test task",
                "",
                "",
                "task",
                1,
                "",
                None,
                None,
            )
            .await
            .unwrap();

        let handle = spawn_coordinator_with_planner(&db, &tx);
        handle.trigger_planner_patrol().await.unwrap();
        // Give the actor time to process.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify a review task was created (the patrol creates one for visibility).
        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let tasks_by_project = task_repo.list_by_project(&project.id).await.unwrap();
        assert!(
            tasks_by_project
                .iter()
                .any(|t| t.issue_type == "review" && t.title.contains("patrol")),
            "patrol should create a review task; found tasks: {:?}",
            tasks_by_project
                .iter()
                .map(|t| (&t.title, &t.issue_type))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_completion_triggers_next_wave_decomposition() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let project = test_helpers::create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let _handle = spawn_coordinator_with_planner(&db, &tx);
        tokio::task::yield_now().await;

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "Batch Completion Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Wait for the first decomposition task.
        let initial_decomp = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            initial_decomp.len(),
            1,
            "should have initial decomposition task"
        );
        let decomp_task = &initial_decomp[0];

        // Manually close the decomposition task (simulating Planner completed wave 1).
        task_repo
            .set_status_with_reason(&decomp_task.id, "closed", Some("completed"))
            .await
            .unwrap();

        // Create 2 worker tasks under the epic.
        let w1 = task_repo
            .create(
                &epic.id,
                "Worker Task 1",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
            )
            .await
            .unwrap();
        let w2 = task_repo
            .create(
                &epic.id,
                "Worker Task 2",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Close both worker tasks — this should trigger batch-completion detection.
        task_repo
            .set_status_with_reason(&w1.id, "closed", Some("completed"))
            .await
            .unwrap();
        task_repo
            .set_status_with_reason(&w2.id, "closed", Some("completed"))
            .await
            .unwrap();

        // Wait for the coordinator to create the next-wave decomposition task.
        let next_wave = wait_for_decomp_tasks(&db, &tx, &epic.id, 1).await;
        assert_eq!(
            next_wave.len(),
            1,
            "batch completion should create exactly one new decomposition task"
        );
    }
}
