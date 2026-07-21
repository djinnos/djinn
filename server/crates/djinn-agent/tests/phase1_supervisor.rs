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
//!   3. Zero `.task-runtime/worktrees/` directories materialize anywhere under the
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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use djinn_agent::context::{AgentContext, ReconciliationSweepConfig};
use djinn_agent::file_time::FileTime;
use djinn_agent::lsp::LspManager;
use djinn_agent::roles::RoleRegistry;
use djinn_agent::supervisor::{
    SupervisorError, SupervisorFlow, TaskRunOutcome, TaskRunSpec, TaskRunSupervisor,
    services_for_agent_context, services_for_agent_context_with_provider_override,
};
use djinn_core::events::EventBus;
use djinn_core::models::TaskRunTrigger;
use djinn_db::{
    CreateTaskAttemptParams, Database, EffectiveCreatorProvenance, EpicCreateInput, EpicRepository,
    ProjectRepository, SessionMessageRepository, SessionRepository, TaskAttemptRepository,
    TaskRepository, TaskRunRepository, UserRepository,
};
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_provider::message::{ContentBlock, Conversation, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_workspace::MirrorManager;
use futures::stream;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

// ──────────────────────────────────────────────────────────────────────────────
// Test fixtures (inlined because `djinn_agent::test_helpers` is `#[cfg(test)]`
// which does not cross the integration-test compilation unit boundary.)
// ──────────────────────────────────────────────────────────────────────────────

static NEXT_FIXTURE_GITHUB_ID: AtomicI64 = AtomicI64::new(9_400_000_000);

fn test_agent_context(db: Database) -> AgentContext {
    AgentContext {
        db,
        event_bus: EventBus::noop(),
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        background_work_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
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
        runtime_ops: None,
        cargo_target_runs_root: Some({
            let path = std::env::current_dir()
                .unwrap()
                .join("target")
                .join("test-tmp")
                .join(format!("cargo-target-runs-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&path).unwrap();
            path
        }),
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        read_source_authorization: djinn_agent::context::ReadSourceAuthorization::default(),
        reconciliation_sweep: ReconciliationSweepConfig::default(),
        memory_intent_planner: djinn_agent::context::MemoryIntentPlannerConfig::default(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        compaction_cs: djinn_slot::reply_loop::CompactionCriticalSection::default(),
    }
}

async fn create_dispatch_attempt(db: &Database, task_id: &str) -> String {
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("phase1-supervisor-{attempt_id}");
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id,
            role: "worker",
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create exact dispatch attempt")
        .id
}

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    fields: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct EventCaptureLayer {
    events: Arc<StdMutex<Vec<CapturedEvent>>>,
}

impl EventCaptureLayer {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("event capture mutex").clone()
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: HashMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S> Layer<S> for EventCaptureLayer
where
    S: tracing::Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("event capture mutex")
            .push(CapturedEvent {
                fields: visitor.fields,
            });
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

/// Walk `root` recursively and assert no `.task-runtime/worktrees` directory exists.
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
            // Match `.task-runtime/worktrees` as an adjacent pair to avoid
            // false positives from a bare directory named "worktrees".
            if path.file_name().and_then(|n| n.to_str()) == Some("worktrees")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some(".task-runtime")
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
        "expected no .task-runtime/worktrees under {}; found: {hits:?}",
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
                blocked_by: None,
            },
        )
        .await
        .expect("create epic");
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let github_id = NEXT_FIXTURE_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    let creator = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("phase1-supervisor-fixture-{github_id}"),
            Some("Phase 1 Supervisor Fixture"),
            None,
        )
        .await
        .expect("create task creator");
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
    let supervisor = TaskRunSupervisor::new(mirror.clone(), services);

    // 5. Spike flow = single Architect stage — minimizes reply_loop surface.
    let task_attempt_id = create_dispatch_attempt(&db, &task.id).await;
    let spec = TaskRunSpec {
        task_run_id: uuid::Uuid::now_v7().to_string(),
        task_attempt_id: Some(task_attempt_id),
        task_id: task.id.clone(),
        project_id: project.id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/phase1-test".into(),
        flow: SupervisorFlow::Spike,
        model_id_per_role: Default::default(),
        read_source_project_ids: Vec::new(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
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
        Err(other) => panic!("unexpected supervisor error (expected Stage or Ok): {other:?}"),
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
    system_prompts: Arc<StdMutex<Vec<String>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: Arc::new(StdMutex::new(turns.into_iter().collect())),
            system_prompts: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    fn system_prompts(&self) -> Vec<String> {
        self.system_prompts
            .lock()
            .expect("recorded system prompts mutex")
            .clone()
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted-phase1-stub"
    }

    fn stream<'a>(
        &'a self,
        conversation: &'a Conversation,
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
        let system_prompts = Arc::clone(&self.system_prompts);
        let system_prompt = conversation
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .map(|message| message.text_content())
            .expect("provider receives a rendered system prompt");
        Box::pin(async move {
            system_prompts
                .lock()
                .expect("recorded system prompts mutex")
                .push(system_prompt);
            let events = turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedProvider script exhausted");
            let iter = events.into_iter().map(Ok);
            Ok(Box::pin(stream::iter(iter))
                as Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
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
                blocked_by: None,
            },
        )
        .await
        .expect("create epic");
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    // `spike` issue_type so the coordinator-side flow-for-task rules would
    // also pick SupervisorFlow::Spike — we set the spec.flow explicitly below
    // regardless, but keep the row consistent.
    let github_id = NEXT_FIXTURE_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    let creator = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("phase1-supervisor-fixture-{github_id}"),
            Some("Phase 1 Supervisor Fixture"),
            None,
        )
        .await
        .expect("create task creator");
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
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
    let supervisor = TaskRunSupervisor::new(mirror.clone(), services);

    let task_attempt_id = create_dispatch_attempt(&db, &task.id).await;
    let spec = TaskRunSpec {
        task_run_id: uuid::Uuid::now_v7().to_string(),
        task_attempt_id: Some(task_attempt_id),
        task_id: task.id.clone(),
        project_id: project.id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/phase1-stub".into(),
        flow: SupervisorFlow::Spike,
        model_id_per_role: Default::default(),
        read_source_project_ids: Vec::new(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    };

    // 4. Drive the run — with the provider stubbed, the architect stage
    //    finalizes via `submit_work` and the Spike flow maps that to
    //    TaskRunOutcome::Closed (see `mod.rs::run_sequence`'s Spike/Planning
    //    tail branch).
    // The session-start event is structured tracing telemetry. Serialize this
    // collector because tracing's default dispatcher is thread-local and this
    // integration test may run alongside other telemetry tests.
    static TRACE_CAPTURE_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    let _trace_capture_guard = TRACE_CAPTURE_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    let telemetry = EventCaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(telemetry.clone());
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let report = match supervisor.run(spec).await {
        Ok(r) => r,
        Err(e) => panic!("supervisor run failed: {e:?}"),
    };
    let captured_events = telemetry.events();

    // ── Outcome assertions ────────────────────────────────────────────────────
    assert!(
        !report.task_run_id.is_empty(),
        "report.task_run_id should be populated"
    );
    match &report.outcome {
        TaskRunOutcome::Closed { .. } => {}
        other => panic!("expected TaskRunOutcome::Closed from Spike flow; got {other:?}"),
    }

    // ── (b) task_runs.status row is terminal ──────────────────────────────────
    let run_id = assert_task_run_with_status(task_runs.as_ref(), &task.id, &["completed"]).await;
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
    assert_eq!(
        architect_session.project_id.as_deref(),
        Some(project.id.as_str())
    );
    assert_eq!(architect_session.task_id.as_deref(), Some(task.id.as_str()));

    // The event must identify this exact persisted session/task/role and hash
    // the exact prompt handed to the provider, rather than an earlier render.
    let session_start = captured_events
        .iter()
        .find(|event| event.fields.get("event").map(String::as_str) == Some("session_start"))
        .expect("structured session_start telemetry");
    let prompt = stub
        .system_prompts()
        .into_iter()
        .next()
        .expect("fake provider received one system prompt");
    let expected_prompt_hash = djinn_roles::prompts::rendered_system_prompt_hash(&prompt);
    let prompt_hash = session_start
        .fields
        .get("prompt_hash")
        .expect("session_start prompt hash");
    assert_eq!(prompt_hash, &expected_prompt_hash);
    assert!(
        prompt_hash.starts_with("sha256:")
            && prompt_hash.len() == "sha256:".len() + 16
            && prompt_hash["sha256:".len()..]
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()),
        "prompt hash must use the sha256:<16 lowercase hex> format: {prompt_hash}"
    );
    assert_eq!(
        session_start.fields.get("session_id").map(String::as_str),
        Some(architect_session.id.as_str())
    );
    assert_eq!(
        session_start.fields.get("task_id").map(String::as_str),
        Some(task.short_id.as_str())
    );
    assert_eq!(
        session_start.fields.get("agent_type").map(String::as_str),
        Some("architect")
    );
    assert_eq!(
        session_start
            .fields
            .get("prompt_hash_input")
            .map(String::as_str),
        Some("rendered_system_prompt_v1")
    );

    // The real reply loop persists its downstream assistant behavior under the
    // same session id emitted in session-start telemetry.
    let message_repo = SessionMessageRepository::new(db.clone(), events.clone());
    let persisted_behavior = message_repo
        .load_for_sessions(std::slice::from_ref(&architect_session.id))
        .await
        .expect("load persisted session behavior");
    assert!(
        persisted_behavior.iter().any(|(session_id, role, _, _)| {
            session_id == &architect_session.id && role == "assistant"
        }),
        "the assistant behavior record must carry the session-start session id"
    );

    // ── (c) no worktrees anywhere under the test-controlled roots ────────────
    assert_no_worktrees(source_dir.path());
    assert_no_worktrees(mirrors_dir.path());
}

// ──────────────────────────────────────────────────────────────────────────────
// Proactive dispatch-time sync: a NewTask dispatch whose task branch is BEHIND
// a (non-conflicting) advanced base must merge the base into the task branch and
// push the merge to the mirror — even though the stubbed worker makes no edits.
// Observable via the mirror's task_branch (the supervisor's eager push lands the
// merge commit there). Asserts `origin/main` is an ancestor of the task branch
// tip after the run.
// ──────────────────────────────────────────────────────────────────────────────

/// Read `git rev-parse <rev>` in `dir`.
async fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(dir)
        .output()
        .await
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "git rev-parse {rev} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `git merge-base --is-ancestor A B` → true iff A is an ancestor of B.
async fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", a, b])
        .current_dir(dir)
        .output()
        .await
        .expect("git merge-base");
    match out.status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("git merge-base --is-ancestor {a} {b} exited {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proactive_sync_merges_advanced_base_into_behind_task_branch() {
    // 1. Source repo + mirror.
    let source_dir = TempDir::new().unwrap();
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    let mirrors_dir = TempDir::new().unwrap();
    let mirror = Arc::new(MirrorManager::new(mirrors_dir.path().to_path_buf()));

    let db = Database::open_in_memory().expect("open_in_memory test db");
    let events = EventBus::noop();
    let project_repo = ProjectRepository::new(db.clone(), events.clone());
    let project = project_repo
        .create("sync-test", "test", "sync-test")
        .await
        .expect("create project row");

    mirror
        .ensure_mirror(&project.id, &source_url)
        .await
        .expect("ensure_mirror");
    let mirror_path = mirror.mirror_path(&project.id);

    // 2. Stand up the task branch in the mirror at main's CURRENT tip (the
    //    "prior cycle" commit), then advance the mirror's main with a
    //    non-conflicting commit so the task branch is genuinely behind base.
    let task_branch = "djinn/sync-behind";
    // Cut task_branch from main's tip inside the bare mirror.
    run_git(&["git", "branch", task_branch, "main"], &mirror_path).await;

    // Advance main via a throwaway clone touching a NEW file (no conflict), then
    // push back to the mirror's main.
    let pusher = TempDir::new().unwrap();
    run_git(
        &["git", "clone", mirror_path.to_str().unwrap(), "."],
        pusher.path(),
    )
    .await;
    run_git(&["git", "config", "user.email", "t@t"], pusher.path()).await;
    run_git(&["git", "config", "user.name", "t"], pusher.path()).await;
    run_git(&["git", "checkout", "main"], pusher.path()).await;
    tokio::fs::write(pusher.path().join("base_new.txt"), "from-main\n")
        .await
        .unwrap();
    run_git(&["git", "add", "-A"], pusher.path()).await;
    run_git(&["git", "commit", "-m", "base advances"], pusher.path()).await;
    run_git(&["git", "push", "origin", "main"], pusher.path()).await;

    let main_tip = rev_parse(&mirror_path, "main").await;
    let task_tip_before = rev_parse(&mirror_path, task_branch).await;
    assert!(
        !is_ancestor(&mirror_path, &main_tip, &task_tip_before).await,
        "precondition: advanced main must NOT yet be an ancestor of the task branch"
    );

    // 3. Task row.
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let epic = epic_repo
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "sync-epic",
                description: "sync epic",
                emoji: "🧪",
                color: "purple",
                owner: "test-owner",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("create epic");
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let github_id = NEXT_FIXTURE_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    let creator = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("phase1-proactive-sync-fixture-{github_id}"),
            Some("Phase 1 Proactive Sync Fixture"),
            None,
        )
        .await
        .expect("create task creator");
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
            "sync-task",
            "sync task description",
            "sync task design",
            "task",
            2,
            "test-owner",
            None,
            None,
        )
        .await
        .expect("create task");

    // 4. Stub the worker (and reviewer) provider: the worker finalizes with NO
    //    file edits, so the ONLY thing that can advance the task branch is the
    //    proactive sync's merge commit + eager push.
    let worker_turn = vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "worker-fin-1".into(),
            name: "submit_work".into(),
            input: serde_json::json!({
                "task_id": task.short_id,
                "summary": "no edits; just sync",
            }),
        }),
        StreamEvent::Done,
    ];
    let reviewer_turn = vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "reviewer-fin-1".into(),
            name: "submit_review".into(),
            input: serde_json::json!({
                "task_id": task.short_id,
                "decision": "approve",
                "summary": "lgtm",
            }),
        }),
        StreamEvent::Done,
    ];
    let stub = Arc::new(ScriptedProvider::new(vec![worker_turn, reviewer_turn]));

    let cancel = CancellationToken::new();
    let agent_ctx = test_agent_context(db.clone());
    let services = services_for_agent_context_with_provider_override(
        agent_ctx,
        cancel.clone(),
        stub.clone() as Arc<dyn LlmProvider>,
    );
    let supervisor = TaskRunSupervisor::new(mirror.clone(), services);

    let task_attempt_id = create_dispatch_attempt(&db, &task.id).await;
    let spec = TaskRunSpec {
        task_run_id: uuid::Uuid::now_v7().to_string(),
        task_attempt_id: Some(task_attempt_id),
        task_id: task.id.clone(),
        project_id: project.id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: task_branch.into(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: Default::default(),
        read_source_project_ids: Vec::new(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    };

    // 5. Drive the run. A supervisor error before the proactive-sync block is
    //    a fixture failure, not a topology failure, so surface it directly.
    //    Successful terminal outcomes may vary with the stubbed stages; the git
    //    assertions below remain the behavioral contract under test.
    supervisor
        .run(spec)
        .await
        .expect("fixture must reach proactive dispatch-time sync");

    // 6. Mirror's task branch tip now has the advanced main as an ancestor.
    let task_tip_after = rev_parse(&mirror_path, task_branch).await;
    assert_ne!(
        task_tip_before, task_tip_after,
        "proactive sync should have advanced the mirror's task branch with a merge commit"
    );
    assert!(
        is_ancestor(&mirror_path, &main_tip, &task_tip_after).await,
        "after proactive sync, origin/main must be an ancestor of the task branch tip"
    );

    assert_no_worktrees(source_dir.path());
    assert_no_worktrees(mirrors_dir.path());
}
