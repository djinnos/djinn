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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use djinn_agent::context::AgentContext;
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_agent::roles::RoleRegistry;
use djinn_agent::supervisor::{
    SupervisorError, SupervisorFlow, TaskRunOutcome, TaskRunSpec, TaskRunSupervisor,
    services_for_agent_context, services_for_agent_context_with_provider_override,
};
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::{
    Database, EpicCreateInput, EpicRepository, ProjectRepository, SessionRepository,
    TaskRepository, TaskRunRepository,
};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_workspace::MirrorManager;
use futures::stream;
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
        graph_warmer: None,
        repo_graph_ops: None,
        mirror: None,
        rpc_registry: None,
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
    // Project paths are now derived from (github_owner, github_repo) at
    // runtime, not persisted, so the supervisor doesn't read project.path
    // directly. Use a deterministic slug for the fixture.
    let project = project_repo
        .create("phase1-test", "test", "phase1-test")
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
    let services = services_for_agent_context(agent_ctx, cancel.clone());
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

// ──────────────────────────────────────────────────────────────────────────────
// Stub LlmProvider — drives the supervisor Spike flow to completion.
// ──────────────────────────────────────────────────────────────────────────────

/// A trivial scripted provider that returns pre-recorded stream events.
///
/// Inlined here because `djinn_agent::test_helpers::FakeProvider` is gated on
/// `#[cfg(test)]` in the crate and is therefore not visible to this
/// integration-test compilation unit.  We only need enough fidelity to steer
/// one architect stage — a single turn that emits a `submit_work` tool call
/// which the reply loop recognises as a finalize.
struct ScriptedProvider {
    turns: Arc<StdMutex<VecDeque<Vec<StreamEvent>>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: Arc::new(StdMutex::new(turns.into_iter().collect())),
        }
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted-phase1-stub"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [serde_json::Value],
        _tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let turns = Arc::clone(&self.turns);
        Box::pin(async move {
            let events = turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedProvider script exhausted");
            let iter = events.into_iter().map(Ok);
            Ok(Box::pin(stream::iter(iter))
                as Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>)
        })
    }
}

/// Assert a task_runs row exists for `task_id` and its status matches one of
/// the allowed values.  Returns the run id.
async fn assert_task_run_with_status(
    task_runs: &TaskRunRepository,
    task_id: &str,
    allowed_statuses: &[&str],
) -> String {
    let runs = task_runs
        .list_for_task(task_id)
        .await
        .expect("list task_runs");
    assert_eq!(
        runs.len(),
        1,
        "expected exactly one task_run row for task {task_id}, got {}",
        runs.len()
    );
    let run = &runs[0];
    assert!(
        allowed_statuses.contains(&run.status.as_str()),
        "task_run.status = {} (expected one of {:?})",
        run.status,
        allowed_statuses
    );
    run.id.clone()
}

// ──────────────────────────────────────────────────────────────────────────────
// Full-fidelity e2e test: Spike flow runs through the supervisor, stubbed LLM
// emits a `submit_work` finalize, supervisor reaches TaskRunOutcome::Closed,
// sessions child row has task_run_id FK, no worktrees anywhere.
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_spike_runs_to_close_with_stubbed_provider() {
    // 1. Source repo + mirror (identical bootstrap to the infrastructure test).
    let source_dir = TempDir::new().unwrap();
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    let mirrors_dir = TempDir::new().unwrap();
    let mirror = Arc::new(MirrorManager::new(mirrors_dir.path().to_path_buf()));

    let db = Database::open_in_memory().expect("open_in_memory test db");
    let events = EventBus::noop();
    let project_repo = ProjectRepository::new(db.clone(), events.clone());
    let project = project_repo
        .create("phase1-stub-test", "test", "phase1-stub-test")
        .await
        .expect("create project row");

    mirror
        .ensure_mirror(&project.id, &source_url)
        .await
        .expect("ensure_mirror");

    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let epic = epic_repo
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "phase1-stub-epic",
                description: "phase1 stub epic",
                emoji: "🧪",
                color: "green",
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
    // `spike` issue_type so the coordinator-side flow-for-task rules would
    // also pick SupervisorFlow::Spike — we set the spec.flow explicitly below
    // regardless, but keep the row consistent.
    let task = task_repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "phase1-stub-task",
            "phase1 stub task description",
            "phase1 stub task design",
            "spike",
            2,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create task");

    // 2. Script the stubbed provider: a single turn that emits a
    //    `submit_work` tool-use block.  The reply loop recognises this as
    //    the architect's finalize tool (see `ARCHITECT_CONFIG::
    //    finalize_tool_names`) and exits cleanly.
    let stub = Arc::new(ScriptedProvider::new(vec![vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "architect-fin-1".into(),
            name: "submit_work".into(),
            input: serde_json::json!({
                "task_id": task.short_id,
                "summary": "phase1 stub: no changes",
            }),
        }),
        StreamEvent::Done,
    ]]));

    // 3. Supervisor services wired with the provider override.
    let cancel = CancellationToken::new();
    let agent_ctx = test_agent_context(db.clone());
    let task_runs = Arc::new(TaskRunRepository::new(db.clone()));
    let services = services_for_agent_context_with_provider_override(
        agent_ctx,
        cancel.clone(),
        stub.clone() as Arc<dyn LlmProvider>,
    );
    let supervisor = TaskRunSupervisor::new(task_runs.clone(), mirror.clone(), services);

    let spec = TaskRunSpec {
        task_id: task.id.clone(),
        project_id: project.id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/phase1-stub".into(),
        flow: SupervisorFlow::Spike,
        model_id_per_role: Default::default(),
    };

    // 4. Drive the run — with the provider stubbed, the architect stage
    //    finalizes via `submit_work` and the Spike flow maps that to
    //    TaskRunOutcome::Closed (see `mod.rs::run_sequence`'s Spike/Planning
    //    tail branch).
    let report = match supervisor.run(spec).await {
        Ok(r) => r,
        Err(e) => panic!("supervisor run failed: {e:?}"),
    };

    // ── Outcome assertions ────────────────────────────────────────────────────
    assert!(
        !report.task_run_id.is_empty(),
        "report.task_run_id should be populated"
    );
    match &report.outcome {
        TaskRunOutcome::Closed { .. } => {}
        other => panic!(
            "expected TaskRunOutcome::Closed from Spike flow; got {other:?}"
        ),
    }

    // ── (b) task_runs.status row is terminal ──────────────────────────────────
    let run_id = assert_task_run_with_status(
        task_runs.as_ref(),
        &task.id,
        &["completed"],
    )
    .await;
    assert_eq!(run_id, report.task_run_id, "run_id round-trips");

    // ── (a) child sessions row exists with task_run_id FK populated ──────────
    let session_repo = SessionRepository::new(db.clone(), events.clone());
    let sessions = session_repo
        .list_for_task(&task.id)
        .await
        .expect("list sessions for task");
    assert!(
        !sessions.is_empty(),
        "expected at least one session row for the task-run"
    );
    let architect_session = sessions
        .iter()
        .find(|s| s.agent_type == "architect")
        .expect("expected an architect session row");
    assert_eq!(
        architect_session.task_run_id.as_deref(),
        Some(report.task_run_id.as_str()),
        "session.task_run_id FK must point at the run we just drove"
    );
    assert_eq!(architect_session.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(
        architect_session.task_id.as_deref(),
        Some(task.id.as_str())
    );

    // ── (c) no worktrees anywhere under the test-controlled roots ────────────
    assert_no_worktrees(source_dir.path());
    assert_no_worktrees(mirrors_dir.path());
}
