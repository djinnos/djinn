use super::*;

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
    let (task, _project_path) = create_simple_task(&db, &tx, "review", "commented review").await;

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
    let (task, project_path) = create_simple_task(&db, &tx, "spike", "architect ADR spike").await;

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
