use super::*;
use std::collections::HashMap as StdHashMap;
use std::sync::{Arc as StdArc, Mutex as StdMutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

#[derive(Clone, Debug, Default)]
struct RecordedSpan {
    name: String,
    fields: StdHashMap<String, String>,
}

#[derive(Clone, Default)]
struct RecordingLayer {
    spans: StdArc<StdMutex<Vec<RecordedSpan>>>,
}

impl RecordingLayer {
    fn spans(&self) -> Vec<RecordedSpan> {
        self.spans.lock().expect("recorded spans mutex").clone()
    }
}

#[derive(Default)]
struct FieldRecorder {
    fields: StdHashMap<String, String>,
}

impl Visit for FieldRecorder {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }
}

impl<S> Layer<S> for RecordingLayer
where
    S: tracing::Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let mut recorder = FieldRecorder::default();
        attrs.record(&mut recorder);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(RecordedSpan {
                name: attrs.metadata().name().to_owned(),
                fields: recorder.fields,
            });
        }
    }

    fn on_record(&self, id: &tracing::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut recorder = FieldRecorder::default();
            values.record(&mut recorder);
            if let Some(recorded) = span.extensions_mut().get_mut::<RecordedSpan>() {
                recorded.fields.extend(recorder.fields);
            }
        }
    }

    fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id)
            && let Some(recorded) = span.extensions().get::<RecordedSpan>()
        {
            self.spans
                .lock()
                .expect("recorded spans mutex")
                .push(recorded.clone());
        }
    }
}

fn test_dispatch_pause(reason: &str) -> djinn_core::models::DispatchPause {
    djinn_core::models::DispatchPause {
        paused_by: "coordinator-test".to_owned(),
        paused_at: rfc3339(::time::OffsetDateTime::now_utc()),
        reason: reason.to_owned(),
        expires_at: None,
    }
}

async fn pause_dispatch(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    target: djinn_db::DispatchPauseTarget,
    reason: &str,
) {
    djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .pause(target, test_dispatch_pause(reason))
        .await
        .unwrap();
}

async fn open_task(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    title: &str,
) -> djinn_core::models::Task {
    let (task, _project_path) = create_simple_task(db, tx, "task", title).await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .set_status(&task.id, "open")
        .await
        .unwrap()
}

async fn set_task_creator(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    task: &djinn_core::models::Task,
    github_id: i64,
    login: &str,
) -> djinn_core::models::Task {
    let user = djinn_db::UserRepository::new(db.clone())
        .upsert_from_github(github_id, login, None, None)
        .await
        .unwrap();
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    repo.set_created_by_user_id(&task.id, &user.id)
        .await
        .unwrap();
    repo.get(&task.id).await.unwrap().unwrap()
}

async fn assert_task_status(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    task: &djinn_core::models::Task,
    expected: &str,
) {
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, expected, "task {} status", task.short_id);
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_span_records_task_and_model_fields() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);
    let model_ids = vec!["test-provider/test-model".to_owned()];
    let layer = RecordingLayer::default();
    let subscriber = tracing_subscriber::registry().with(layer.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let outcome = actor
        .try_dispatch_to_pool(
            "span-task",
            "worker",
            7,
            None,
            &model_ids,
            |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
        )
        .await;

    assert!(matches!(outcome, DispatchOutcome::Dispatched));
    let dispatch_span = layer
        .spans()
        .into_iter()
        .find(|span| span.name == "djinn.dispatch")
        .expect("djinn.dispatch span recorded");
    assert_eq!(
        dispatch_span.fields.get("task_id").map(String::as_str),
        Some("span-task")
    );
    assert_eq!(
        dispatch_span.fields.get("model_id").map(String::as_str),
        Some("test-provider/test-model")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_dispatch_pause_defers_ready_task_and_resume_dispatches_same_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let task = open_task(&db, &tx, "global paused task").await;

    pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::global(),
        "global maintenance",
    )
    .await;
    let pause_repo =
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    assert!(pause_repo.get_status().await.unwrap().global.is_some());

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(actor.dispatched, 0, "global pause must suppress dispatch");
    assert!(!actor.last_dispatched.contains_key(&task.id));
    assert!(!actor.dispatch_failure_streak.contains_key(&task.id));
    assert!(!actor.dispatch_cooldowns.contains_key(&task.id));
    assert_task_status(&db, &tx, &task, "open").await;

    pause_repo
        .resume(djinn_db::DispatchPauseTarget::global())
        .await
        .unwrap();
    assert!(pause_repo.get_status().await.unwrap().global.is_none());

    actor.dispatch_ready_tasks(None).await;

    assert_eq!(actor.dispatched, 1, "resumed task should dispatch");
    assert!(actor.last_dispatched.contains_key(&task.id));
    assert_task_status(&db, &tx, &task, "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_dispatch_pause_defers_only_matching_project() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let paused_task = open_task(&db, &tx, "paused project task").await;

    pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::project(paused_task.project_id.clone()),
        "project maintenance",
    )
    .await;
    let pause_state =
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap();
    assert!(pause_state.projects.contains_key(&paused_task.project_id));

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(actor.dispatched, 0, "paused project task must not dispatch");
    assert!(!actor.last_dispatched.contains_key(&paused_task.id));
    assert!(!actor.dispatch_failure_streak.contains_key(&paused_task.id));
    assert!(!actor.dispatch_cooldowns.contains_key(&paused_task.id));
    assert_task_status(&db, &tx, &paused_task, "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_dispatch_pause_does_not_block_unaffected_project() {
    // Companion to `project_dispatch_pause_defers_only_matching_project`:
    // a task in a project that is NOT paused must dispatch normally even
    // while some other project is paused. Proves the project-pause scope is
    // exact and does not bleed into dispatch for other projects. The pause
    // is keyed by a project id that has no ready task, so the dispatch pass
    // sees exactly one ready task — the unpaused one.
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);

    let paused_project_id = uuid::Uuid::now_v7().to_string();
    pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::project(paused_project_id.clone()),
        "project maintenance",
    )
    .await;
    assert!(
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap()
            .projects
            .contains_key(&paused_project_id)
    );

    let other_task = open_task(&db, &tx, "unpaused project task").await;
    assert!(other_task.project_id != paused_project_id);

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        actor.dispatched, 1,
        "unpaused project task should dispatch even while another project is paused"
    );
    assert!(actor.last_dispatched.contains_key(&other_task.id));
    assert!(!actor.dispatch_failure_streak.contains_key(&other_task.id));
    assert!(!actor.dispatch_cooldowns.contains_key(&other_task.id));
    // The pause state must remain intact — pausing one project never
    // accidentally clears or supersedes the persisted scope.
    assert!(
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap()
            .projects
            .contains_key(&paused_project_id)
    );
    assert_task_status(&db, &tx, &other_task, "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_dispatch_pause_defers_only_matching_creator() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let paused_task = open_task(&db, &tx, "paused user task").await;
    let paused_task = set_task_creator(&db, &tx, &paused_task, 10_001, "paused-user").await;

    pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::user(paused_task.created_by_user_id.clone().unwrap()),
        "user maintenance",
    )
    .await;
    let pause_state =
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap();
    assert!(
        pause_state
            .users
            .contains_key(paused_task.created_by_user_id.as_deref().unwrap())
    );

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(actor.dispatched, 0, "paused user task must not dispatch");
    assert!(!actor.last_dispatched.contains_key(&paused_task.id));
    assert!(!actor.dispatch_failure_streak.contains_key(&paused_task.id));
    assert!(!actor.dispatch_cooldowns.contains_key(&paused_task.id));
    assert_task_status(&db, &tx, &paused_task, "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_dispatch_pause_does_not_block_unaffected_creator() {
    // Companion to `user_dispatch_pause_defers_only_matching_creator`:
    // a task owned by a user that is NOT paused must dispatch normally even
    // while some other user is paused. Proves the user-pause scope is
    // exact and does not bleed into dispatch for other users. The pause is
    // keyed by a synthetic user id with no ready task, so the dispatch pass
    // sees exactly one ready task — the unpaused one.
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);

    let paused_user_id = uuid::Uuid::now_v7().to_string();
    pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::user(paused_user_id.clone()),
        "user maintenance",
    )
    .await;
    assert!(
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap()
            .users
            .contains_key(&paused_user_id)
    );

    let other_task = open_task(&db, &tx, "unpaused user task").await;
    let other_task = set_task_creator(&db, &tx, &other_task, 10_002, "other-user").await;
    assert!(other_task.created_by_user_id.as_deref() != Some(paused_user_id.as_str()));

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        actor.dispatched, 1,
        "unpaused user task should dispatch even while another user is paused"
    );
    assert!(actor.last_dispatched.contains_key(&other_task.id));
    assert!(!actor.dispatch_failure_streak.contains_key(&other_task.id));
    assert!(!actor.dispatch_cooldowns.contains_key(&other_task.id));
    // The pause state must remain intact — pausing one user never
    // accidentally clears or supersedes the persisted scope.
    assert!(
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap()
            .users
            .contains_key(&paused_user_id)
    );
    assert_task_status(&db, &tx, &other_task, "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_dispatch_pause_gates_fresh_coordinator_instance() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let task = open_task(&db, &tx, "persisted global paused task").await;

    pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::global(),
        "restart maintenance",
    )
    .await;
    assert!(
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap()
            .global
            .is_some(),
        "pause fixture must be durable before constructing the fresh coordinator"
    );

    let mut fresh_actor = coordinator_actor_for_tests(&db, &tx);
    fresh_actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        fresh_actor.dispatched, 0,
        "fresh coordinator must reload persisted pause state before dispatch"
    );
    assert!(!fresh_actor.last_dispatched.contains_key(&task.id));
    assert!(!fresh_actor.dispatch_failure_streak.contains_key(&task.id));
    assert!(!fresh_actor.dispatch_cooldowns.contains_key(&task.id));
    assert_task_status(&db, &tx, &task, "open").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_simple_task_without_durable_artifacts_closes_directly() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) = create_simple_task(&db, &tx, "spike", "artifact-free spike").await;

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
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
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
    std::fs::create_dir_all(tmp.path().join("docs/decisions/proposed")).unwrap();
    std::fs::write(
        tmp.path().join("docs/decisions/proposed/adr-999.md"),
        "# new ADR\n",
    )
    .unwrap();

    assert!(
        CoordinatorActor::worktree_has_uncommitted_changes(tmp.path()),
        "untracked docs/decisions/proposed/adr-999.md must be detected"
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
///   - writes a *real* untracked `docs/decisions/proposed/adr-*.md` file,
///   - injects NO synthetic event_taxonomy (the worktree-status signal
///     must be the one that triggers the routing change), and
///   - does NOT pre-create the `task/<short_id>` branch (the whole point
///     of the assertion is that we *route through* the PR flow because
///     the artifact was detected, instead of short-circuiting to close).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn architect_spike_with_real_adr_file_routes_through_pr_flow_via_worktree_signal() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, project_path) = create_simple_task(&db, &tx, "spike", "architect ADR spike").await;

    // Real worktree directory inside the project, initialized as a git repo
    // so git2 status() actually has something to read.
    let worktree_path = Path::new(&project_path)
        .join(".task-runtime")
        .join("worktrees")
        .join(&task.short_id);
    init_git_repo(&worktree_path).await;

    // The architect "writes the ADR" via a shell command — i.e. exactly
    // the kind of change session_extraction.rs would miss because it only
    // counts write/edit/apply_patch tool calls, not call_shell side
    // effects.  We model that here by creating the file directly with std::fs.
    std::fs::create_dir_all(worktree_path.join("docs/decisions/proposed")).unwrap();
    std::fs::write(
        worktree_path.join("docs/decisions/proposed/adr-dtn6-test.md"),
        "# ADR: dtn6 regression coverage\n\nbody body body\n",
    )
    .unwrap();

    // Real session row paired with a task_run row. The coordinator reads
    // the workspace path from `task_runs.workspace_path` (migration 5);
    // migration 6 dropped the legacy `sessions.worktree_path` column.
    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task_run_repo = djinn_db::repositories::task_run::TaskRunRepository::new(db.clone());
    let run_id = uuid::Uuid::now_v7().to_string();
    task_run_repo
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "new_task",
            status: None,
            workspace_path: Some(worktree_path.to_str().unwrap()),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "test-model",
            agent_type: "architect",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
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
