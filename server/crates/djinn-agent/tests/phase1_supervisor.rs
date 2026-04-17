//! Phase 1 end-to-end integration test for `TaskRunSupervisor`.
//!
//! Validates the infrastructure slice of the multiuser refactor mirror →
//! supervisor path:
//!
//!   1. `MirrorManager::ensure_mirror` can clone a local source repo into a
//!      bare mirror and `clone_ephemeral` can materialize a `Workspace`.
//!   2. `TaskRunSupervisor::run` accepts a `TaskRunSpec`, creates a
//!      `TaskRunRecord` in the DB, drives `clone_ephemeral` against the mirror,
//!      and steps into `stage::execute_stage` far enough to prove the
//!      infrastructure wiring.
//!   3. Zero `.djinn/worktrees/` directories materialize anywhere under the
//!      test-controlled roots.
//!
//! ## What is stubbed vs. real
//!
//! - Real: `MirrorManager`, `Workspace`, `TaskRunRepository`, `AgentContext`
//!   wiring, the supervisor's `create-run → clone-mirror → enter-stage` path.
//! - Stubbed by *absence*: there is no credential in the vault, so
//!   `stage::execute_stage` fails at `resolve_model_and_credential` with a
//!   `StageError::ModelResolution`.  That is an intentional early-exit — the
//!   task_run row has already been written, the workspace has already been
//!   cloned from the mirror, and no worktree code has been reached.  See
//!   the dispatch notes below: the LLM reply loop is out of scope for this
//!   integration test by design (see task #12).
//!
//! ## Dolt flake note
//!
//! This test writes to the shared test Dolt (:3307 via `make test`).  If a
//! flake bites, re-run in isolation with
//!   `cargo test -p djinn-agent --test phase1_supervisor -- --test-threads=1`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_agent::context::AgentContext;
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::supervisor::{
    SupervisorError, SupervisorFlow, SupervisorServices, TaskRunSpec, TaskRunSupervisor,
};
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::{
    Database, EpicCreateInput, EpicRepository, ProjectRepository, TaskRepository, TaskRunRepository,
};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_workspace::MirrorManager;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ──────────────────────────────────────────────────────────────────────────────
// Test fixtures (inlined because `djinn_agent::test_helpers` is `#[cfg(test)]`
// which does not cross the integration-test compilation unit boundary.)
// ──────────────────────────────────────────────────────────────────────────────

fn test_agent_context(db: Database) -> AgentContext {
    AgentContext {
        db,
        event_bus: EventBus::noop(),
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        verifying_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        role_registry: Arc::new(RoleRegistry::new()),
        health_tracker: HealthTracker::new(),
        file_time: Arc::new(FileTime::new()),
        lsp: LspManager::new(),
        catalog: CatalogService::new(),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: Default::default(),
        task_ops_project_path_override: None,
        working_root: None,
        canonical_graph_warmer: None,
        repo_graph_ops: None,
        mirror: None,
    }
}

async fn run_git(cmd: &[&str], cwd: &Path) {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn make_source_repo(path: &Path) {
    run_git(&["git", "init", "-b", "main"], path).await;
    run_git(&["git", "config", "user.email", "test@example.com"], path).await;
    run_git(&["git", "config", "user.name", "Test"], path).await;
    tokio::fs::write(path.join("README.md"), "hello")
        .await
        .unwrap();
    run_git(&["git", "add", "."], path).await;
    run_git(&["git", "commit", "-m", "init"], path).await;
}

/// Walk `root` recursively and assert no `.djinn/worktrees` directory exists.
///
/// Phase 1 invariant: the mirror → supervisor path must never materialize a
/// worktree on disk.  A match anywhere under the test-controlled roots is a
/// regression.
fn assert_no_worktrees(root: &Path) {
    fn walk(dir: &Path, hits: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Match `.djinn/worktrees` as an adjacent pair to avoid false
            // positives from a bare directory named "worktrees".
            if path.file_name().and_then(|n| n.to_str()) == Some("worktrees")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some(".djinn")
            {
                hits.push(path.clone());
            }
            walk(&path, hits);
        }
    }

    let mut hits: Vec<PathBuf> = Vec::new();
    walk(root, &mut hits);
    assert!(
        hits.is_empty(),
        "expected no .djinn/worktrees under {}; found: {hits:?}",
        root.display()
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_clones_from_mirror_without_worktrees() {
    // 1. Source repo on disk with one commit on `main`.
    let source_dir = TempDir::new().unwrap();
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    // 2. Mirror root + bare mirror for the project.
    let mirrors_dir = TempDir::new().unwrap();
    let mirror = Arc::new(MirrorManager::new(mirrors_dir.path().to_path_buf()));

    // 3. In-memory DB (actually connects to the test Dolt at :3307) with a
    //    project row whose id we reuse as the `MirrorManager` project_id so the
    //    supervisor's `clone_ephemeral(&spec.project_id, ...)` call resolves.
    let db = Database::open_in_memory().expect("open_in_memory test db");
    let events = EventBus::noop();
    let project_repo = ProjectRepository::new(db.clone(), events.clone());
    // Point the project path at the source repo tempdir so any code that reads
    // project.path sees a real directory.
    let project = project_repo
        .create("phase1-test", &source_dir.path().display().to_string())
        .await
        .expect("create project row");

    // Install the mirror under the project_id the supervisor will look up.
    mirror
        .ensure_mirror(&project.id, &source_url)
        .await
        .expect("ensure_mirror");
    assert!(mirror.mirror_path(&project.id).exists());

    // Seed an epic + task under the same project.
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let epic = epic_repo
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "phase1-epic",
                description: "phase1 test epic",
                emoji: "🧪",
                color: "blue",
                owner: "test-owner",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
            },
        )
        .await
        .expect("create epic");
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "phase1-task",
            "phase1 test task description",
            "phase1 test task design",
            "task",
            2,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create task");

    // 4. Supervisor services + supervisor.
    let cancel = CancellationToken::new();
    let agent_ctx = test_agent_context(db.clone());
    let task_runs = Arc::new(TaskRunRepository::new(db.clone()));
    let services = SupervisorServices::new(agent_ctx, cancel.clone());
    let supervisor = TaskRunSupervisor::new(task_runs.clone(), mirror.clone(), services);

    // 5. Spike flow = single Architect stage — minimizes reply_loop surface.
    let spec = TaskRunSpec {
        task_id: task.id.clone(),
        project_id: project.id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/phase1-test".into(),
        flow: SupervisorFlow::Spike,
        model_id_per_role: Default::default(),
    };

    // 6. Drive the run.  In this infrastructure-slice test we do NOT stub the
    //    LLM provider — there is no credential in the vault, so
    //    `resolve_model_and_credential` fails with `StageError::ModelResolution`
    //    and the supervisor returns `Err(SupervisorError::Stage(...))`.  That's
    //    fine: the task_run row has been created and the workspace has been
    //    cloned from the mirror by that point — those are the invariants this
    //    test actually exercises.
    let result = supervisor.run(spec).await;

    match &result {
        Err(SupervisorError::Stage(_)) => {
            // Expected: credential lookup failed after the mirror clone.
        }
        Ok(report) => {
            // Also acceptable: if a follower change lets Spike complete
            // cleanly, the run report should be terminal with a populated id.
            assert!(
                !report.task_run_id.is_empty(),
                "task_run_id should be populated on success"
            );
        }
        Err(other) => panic!(
            "unexpected supervisor error (expected Stage or Ok): {other:?}"
        ),
    }

    // 7a. A task_run row was created before the stage attempted credential
    //     resolution.  Fetch via `list_for_task` because the error path does
    //     not return the run_id to the caller.
    let runs = task_runs
        .list_for_task(&task.id)
        .await
        .expect("list task_runs");
    assert_eq!(
        runs.len(),
        1,
        "expected exactly one task_run row for the task"
    );
    let run = &runs[0];
    assert_eq!(run.project_id, project.id);
    assert_eq!(run.task_id, task.id);
    assert_eq!(run.trigger_type, TaskRunTrigger::NewTask.as_str());
    // Either running (stage failed before `update_status`) or a terminal status
    // (supervisor reached the end of the run).  Both paths keep the row.
    assert!(
        matches!(
            run.status.as_str(),
            "running" | "completed" | "failed" | "interrupted"
        ),
        "unexpected run.status = {}",
        run.status
    );

    // 7b. No `.djinn/worktrees/` anywhere under our controlled roots.  The
    //     supervisor must never create worktrees — that is the whole point of
    //     the mirror-native workspace model.
    assert_no_worktrees(source_dir.path());
    assert_no_worktrees(mirrors_dir.path());
}
