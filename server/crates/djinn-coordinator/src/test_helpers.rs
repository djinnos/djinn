//! Test utilities for djinn-coordinator tests.
//!
//! Mirrors the subset of `djinn_slot::test_helpers` and
//! `djinn_agent::test_helpers` that coordinator tests need.
//! Returns [`SlotContext`] (from djinn-slot) rather than `AgentContext`.

// This module is also exported under `test-support` for cross-crate integration
// fixtures. Keep test-only assertion ergonomics scoped here so enabling that
// feature does not weaken the reliability lints on production modules.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_slot::host::SlotContext;
use djinn_slot::reply_loop::CompactionCriticalSection;

/// Build a `CoordinatorActor` directly — never spawned — together with the
/// `CancellationToken` its slot pool was built with.
///
/// A test that wants a *deterministic* coordinator pass must not spawn one.
/// `CoordinatorHandle::spawn` detaches `CoordinatorActor::run` onto
/// the test runtime, and that task first walks the whole startup path
/// (incarnation registration, three startup reapers, durable-dispatch
/// rehydration, refinement recovery) and then immediately takes its first
/// 30s tick — `tokio::time::interval` fires once straight away — which runs
/// the full `run_tick` sweep. All of that contends for the test database's
/// **four** pooled connections with the test body itself, so a test that
/// spawns a coordinator and then sleeps a fixed number of milliseconds is
/// really asserting against whatever that race happened to produce.
///
/// Driving `handle_event` / `on_task_closed` on an unspawned actor replaces
/// that race with a happens-before edge: the pass is complete when the
/// `.await` returns, so the assertion that follows observes a finished pass
/// rather than an unstarted one.
///
/// The returned token is the one held by the `SlotPoolHandle` spawned
/// here. Nothing else in the process will ever fire it, so a caller
/// that drops it on the floor leaks that pool task for the lifetime of the
/// test binary; cancel it (and close the pool) at the end of the test body:
///
/// ```ignore
/// let (mut actor, cancel) = test_helpers::make_coordinator_actor_cancellable(&db, &tx);
/// // ... drive passes, assert ...
/// cancel.cancel();
/// db.pool().close().await;
/// ```
pub fn make_coordinator_actor_cancellable(
    db: &Database,
    tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
) -> (crate::actor::CoordinatorActor, CancellationToken) {
    use crate::roles::RoleRegistry;
    use crate::types::{
        BackgroundWorkTracker, CoordinatorDeps, DEFAULT_MODEL_ID, SharedCoordinatorState,
    };
    use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use std::collections::HashMap;

    let cancel = CancellationToken::new();
    let ctx = agent_context_from_db(db.clone(), cancel.clone());
    let pool = SlotPoolHandle::spawn(
        ctx,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
    );
    let (status_tx, _) = tokio::sync::watch::channel(SharedCoordinatorState {
        dispatched: 0,
        recovered: 0,
        epic_throughput: HashMap::new(),
        pr_errors: HashMap::new(),
        rate_limited_until: None,
    });
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    let actor = crate::actor::CoordinatorActor::new(
        CoordinatorDeps::new(
            tx.clone(),
            cancel.clone(),
            db.clone(),
            pool,
            CatalogService::new(),
            djinn_provider::catalog::health::HealthTracker::new(),
            Arc::new(RoleRegistry::new()),
            BackgroundWorkTracker::default(),
            djinn_lsp::LspManager::new(),
        ),
        receiver,
        sender,
        status_tx,
    );
    (actor, cancel)
}

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("failed to create tempdir")
}

pub fn test_persistent_dir(prefix: &str) -> PathBuf {
    test_tempdir(prefix).keep()
}

/// Isolated stand-in for `djinn_core::paths::cache_root()` in tests.
///
/// One directory per test binary, so the cache sweeps in
/// `health::sweep_stale_resources` operate on a tempdir instead of the
/// developer's real `~/.djinn/cache`.
pub fn test_cache_root() -> PathBuf {
    static TEST_CACHE_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TEST_CACHE_ROOT
        .get_or_init(|| test_persistent_dir("djinn-test-cache-root-"))
        .clone()
}

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("open in-memory test database")
}

pub fn agent_context_from_db(db: Database, cancel: CancellationToken) -> SlotContext {
    agent_context_from_db_with_clock(db, cancel, Arc::new(djinn_core::clock::SystemClock::new()))
}

/// Like [`agent_context_from_db`] but with an injectable [`djinn_core::clock::Clock`]
/// so tests can advance monotonic time deterministically (e.g. to drive the
/// stall-timeout recovery path without sleeping).
pub fn agent_context_from_db_with_clock(
    db: Database,
    _cancel: CancellationToken,
    clock: Arc<dyn djinn_core::clock::Clock>,
) -> SlotContext {
    let event_bus = EventBus::noop();
    let catalog = CatalogService::new();
    let health_tracker = HealthTracker::default();
    let background_work = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // No-op host callbacks for tests
    struct NoopCallbacks;
    impl djinn_slot::host::SlotHostCallbacks for NoopCallbacks {
        fn interrupt_paused_worker_session<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn resolve_mcp_tools<'a>(
            &'a self,
            _worktree_path: &'a str,
            _role_name: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in test".into()) })
        }
        fn render_prompt(
            &self,
            _role_name: &str,
            _task: &djinn_core::models::Task,
            _context_json: &serde_json::Value,
        ) -> String {
            String::new()
        }
        fn initial_user_message<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async { String::new() })
        }
        fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
            panic!(
                "build_mcp_state not implemented in test NoopCallbacks; \
                 override via a custom SlotHostCallbacks impl if your test needs McpState"
            )
        }
        fn require_project_id_for_task_ops<'a>(
            &'a self,
            _project: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            String,
                            djinn_control_plane::tools::task_tools::ErrorResponse,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                    error: "not implemented".into(),
                })
            })
        }
        fn resolve_provider_credential<'a>(
            &'a self,
            _provider_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<djinn_slot::helpers::ProviderCredential, String>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in test".into()) })
        }
        fn run_task_dispatch<'a>(
            &'a self,
            _task_id: String,
            _execution_generation: i64,
            _project_path: String,
            _model_id: String,
            _ctx: SlotContext,
            _kill: CancellationToken,
            _pause: CancellationToken,
            _resume_lifecycle_metadata: Option<serde_json::Value>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn touch_activity_rpc<'a>(
            &'a self,
            _task_id: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn flush_session_tokens_rpc<'a>(
            &'a self,
            _session_id: String,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    SlotContext {
        db,
        event_bus,
        catalog,
        health_tracker,
        background_work_tasks: background_work,
        active_tasks,
        default_project_id: None,
        working_root: None,
        coordinator_trigger: None,
        runtime_ops: None,
        repo_graph_ops: None,
        clock,
        callbacks: Arc::new(NoopCallbacks),
        tool_dispatcher: None,
        compaction_cs: CompactionCriticalSection::new(),
        live_identity: None,
        model_turn_capability_reporter: None,
    }
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let event_bus = EventBus::noop();
    let repo = djinn_db::ProjectRepository::new(db.clone(), event_bus);
    let uuid = uuid::Uuid::now_v7().simple();
    let project = repo
        .create(
            &format!("test-project-{uuid}"),
            &format!("owner-{uuid}"),
            &format!("repo-{uuid}"),
        )
        .await
        .expect("create project");
    // Satisfy the coordinator's readiness gate so existing tests can dispatch
    // without threading a full devcontainer pipeline.
    let image = djinn_db::ProjectImage {
        tag: Some(format!(
            "test-registry/djinn-project-{}:testhash",
            project.id
        )),
        hash: Some("testhash".into()),
        status: djinn_db::ProjectImageStatus::READY.into(),
        last_error: None,
    };
    let _ = repo.set_project_image(&project.id, &image).await;
    let image_repo = djinn_db::ImageRepository::new(db.clone());
    let image_id = format!(
        "ci-ready-{}",
        &uuid::Uuid::now_v7().simple().to_string()[..16]
    );
    let image_name = format!("ci-ready-{}", &image_id[..8]);
    let _ = image_repo
        .create(
            &image_id,
            &image_name,
            Some("ready test image"),
            r#"{"schema_version":1}"#,
        )
        .await;
    let _ = image_repo
        .mark_ready(
            &image_id,
            image
                .tag
                .as_deref()
                .unwrap_or("test-registry/djinn-test:testhash"),
            Some("sha256:testhash"),
            None,
        )
        .await;
    let _ = image_repo
        .set_project_image(&project.id, Some(&image_id))
        .await;
    let cache_repo = djinn_db::RepoGraphCacheRepository::new(db.clone());
    let _ = cache_repo
        .upsert(djinn_db::RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: "test-commit",
            graph_blob: b"test-graph",
        })
        .await;
    let _ = djinn_db::ProjectWorkspaceGraphRepository::new(db.clone())
        .upsert(djinn_db::ProjectWorkspaceGraphUpsert {
            project_id: &project.id,
            workspace_slug: "root",
            commit_sha: "test-commit",
            status: "ready",
        })
        .await;
    project
}

/// Build a [`CoordinatorContext`] for tests that exercise coordinator-owned
/// health/doctor functions (which take `&CoordinatorContext`, not
/// `&SlotContext`).
pub fn coordinator_context_from_db(
    db: Database,
    _cancel: CancellationToken,
) -> crate::context::CoordinatorContext {
    let event_bus = EventBus::noop();
    let catalog = CatalogService::new();
    let health_tracker = HealthTracker::default();
    let background_work = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let role_registry = Arc::new(crate::roles::RoleRegistry::new());

    crate::context::CoordinatorContext {
        db,
        event_bus,
        git_actors: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        background_work_tasks: background_work,
        role_registry,
        health_tracker,
        file_time: Arc::new(crate::file_time::FileTime::new()),
        lsp: djinn_lsp::LspManager::new(),
        catalog,
        active_tasks,
        task_ops_project_path_override: None,
        working_root: None,
        graph_warmer: None,
        warm_job_guard: None,
        repo_graph_ops: None,
        runtime_ops: None,
        // A test context MUST NOT leave the cache sweeps unrooted.
        //
        // `health::sweep_stale_resources` runs five cache sweeps, and the
        // destructive branches of the sccache and warm-base guards call
        // `remove_dir_all`. With `None` here they resolved
        // `djinn_core::paths::cache_root()`, which on a developer machine is the
        // real `~/.djinn/cache` — so running the coordinator test suite could
        // delete the developer's own build cache. Pin every root under one
        // per-process tempdir instead.
        cargo_target_runs_root: Some(test_cache_root().join("cargo-target-runs")),
        host_cache_root: Some(test_cache_root()),
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        reconciliation_sweep: crate::context::ReconciliationSweepConfig::default(),
        cache_cleanup: crate::context::CacheCleanupConfig::default(),
    }
}

/// Run only startup Stage B (stale task-run reaping) of the startup reaper
/// phase.
///
/// [`crate::complete_startup_reaper_phase`] is the production entry point and
/// is the only caller of the two halves; these re-exports exist so a
/// server-layer regression can snapshot the durable transition table at the
/// Stage A/B and Stage B/C boundaries instead of only at the end of the phase.
/// They call exactly the same functions in exactly the same order, so a
/// regression that also drives `complete_startup_reaper_phase` end to end still
/// detects a dropped stage in the production composition.
pub async fn run_startup_reaper_stage_b(
    db: &Database,
    census: Option<&crate::startup_census::StartupCensus>,
) {
    crate::actor::startup_reaper_stage_b(db, census).await;
}

/// Run only startup Stage C (orphaned pending-attempt classification).
/// See [`run_startup_reaper_stage_b`].
pub async fn run_startup_reaper_stage_c(
    db: &Database,
    coordinator_incarnation_id: &str,
    census: Option<&crate::startup_census::StartupCensus>,
) {
    crate::actor::startup_reaper_stage_c(db, coordinator_incarnation_id, census).await;
}

// ── Startup refinement fixtures ──────────────────────────────────────────
//
// These live here, outside every refinement implementation file, so a
// server-layer regression can drive the production startup census through
// Stage A/B/C against a durable refinement run and then enter the production
// refinement recovery path. Nothing below reimplements refinement behaviour:
// each step calls the same repository or actor entry point production uses.

/// The durable coordinates of one materialized refinement role dispatch.
pub struct StartupRefinementFixture {
    pub proposal_id: String,
    pub project_id: String,
    pub user_id: String,
    pub run_id: String,
    pub generation: i32,
    pub intent_id: String,
    /// The refinement role task created for the claimed intent.
    pub task_id: String,
    /// The task-run whose Kubernetes Job the startup census observes.
    pub task_run_id: String,
    /// The running agent session linked to that task-run.
    pub session_id: String,
}

/// Seed a refinement run that has reached the shape a live role dispatch has:
/// an admitted run, a claimed-then-materialized intent, its correlated role
/// task, and a `running` task-run with a `running` session linked to it.
///
/// `suffix` keeps the durable identities unique across fixtures in one database.
pub async fn seed_startup_refinement_fixture(
    actor: &crate::actor::CoordinatorActor,
    db: &Database,
    suffix: &str,
) -> StartupRefinementFixture {
    use djinn_core::models::TaskRefinementCorrelation;
    use djinn_core::refinement_liveness::{RefinementPhase as DurablePhase, RefinementRole};
    use djinn_db::{
        AcknowledgeRefinementTaskMaterializationRequest, AdmitRefinementRunRequest,
        ClaimRefinementIntentRequest, CreateTaskRunParams, ProposalCreateInput, ProposalRepository,
        RefinementAdmissionOutcome, RefinementAdmissionSource, SessionRepository,
        TaskRunRepository, UserRepository,
    };
    use djinn_provider::repos::CredentialRepository;

    let events = EventBus::noop();
    let project = create_test_project(db).await;
    let github_id = 900_000
        + i64::try_from(uuid::Uuid::now_v7().as_u128() % 1_000_000).expect("bounded github id");
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("startup-refinement-{suffix}-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed refinement owner");
    CredentialRepository::new(db.clone(), events.clone())
        .set_with_owner(
            "test",
            "TEST_API_KEY",
            "owner-test-credential",
            Some(&user.id),
        )
        .await
        .expect("seed owner credential");

    let proposal = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user.id.clone()), async {
            ProposalRepository::new(db.clone(), events.clone())
                .create(ProposalCreateInput {
                    title: "Startup refinement fixture",
                    body: "A durable refinement run observed across a server restart.",
                    acceptance_criteria: Some("[]"),
                    status: Some("building"),
                    body_format: None,
                })
                .await
                .expect("create refinement proposal")
        })
        .await;

    let repo = ProposalRepository::new(db.clone(), events.clone());
    repo.add_target(&proposal.id, &project.id, "primary")
        .await
        .expect("link proposal to project");
    repo.start_refinement_with_owner(&proposal.id, Some(&user.id))
        .await
        .expect("persist refinement owner");

    let (run_id, generation, intent_id) = match repo
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: proposal.id.clone(),
            idempotency_key: format!("startup-refinement-{suffix}"),
            source: RefinementAdmissionSource::ExplicitStart {
                actor: "startup-refinement-fixture".into(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit refinement run")
    {
        RefinementAdmissionOutcome::Admitted {
            run_id,
            generation,
            intent_id,
        }
        | RefinementAdmissionOutcome::Existing {
            run_id,
            generation,
            intent_id,
        } => (run_id, generation, intent_id),
    };

    let owner = format!("startup-refinement-fixture-{suffix}");
    let lease = repo
        .claim_refinement_intent(ClaimRefinementIntentRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            owner: owner.clone(),
            lease_millis: 600_000,
        })
        .await
        .expect("claim refinement intent")
        .expect("acquire refinement lease");

    let correlation = TaskRefinementCorrelation::new(
        run_id.clone(),
        intent_id.clone(),
        i64::from(generation),
        i64::from(lease.round),
        DurablePhase::AdversaryAttack,
        RefinementRole::Adversary,
    )
    .expect("valid refinement correlation");
    let task_id = actor
        .create_refinement_task_with_context_and_correlation(
            &proposal.id,
            "adversary",
            lease.round,
            0,
            "startup refinement fixture",
            None,
            Some(&user.id),
            Some(&correlation),
        )
        .await
        .expect("create correlated refinement role task");
    repo.acknowledge_refinement_task_materialization(
        AcknowledgeRefinementTaskMaterializationRequest {
            run_id: run_id.clone(),
            intent_id: intent_id.clone(),
            generation,
            task_id: task_id.clone(),
            owner,
        },
    )
    .await
    .expect("acknowledge refinement task materialization");

    // The dispatched role runs in a task-run Job; the agent's session is what
    // startup Stage A decides about.
    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task_id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create refinement task run");
    let session_id = SessionRepository::new(db.clone(), events)
        .create(djinn_db::CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task_id),
            model: "test/mock",
            agent_type: "adversary",
            metadata_json: None,
            task_run_id: Some(&task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create refinement role session")
        .id;

    StartupRefinementFixture {
        proposal_id: proposal.id,
        project_id: project.id,
        user_id: user.id,
        run_id,
        generation,
        intent_id,
        task_id,
        task_run_id,
        session_id,
    }
}

/// Drive the production refinement rehydration path.
///
/// Forwards to `CoordinatorActor::recover_interrupted_refinements` with no
/// logic of its own — the same call `CoordinatorActor::run` makes at boot.
pub async fn run_refinement_recovery(actor: &mut crate::actor::CoordinatorActor) {
    actor.recover_interrupted_refinements().await;
}

/// The round the production rehydration path rebuilt for `run_id`, or `None`
/// when it rebuilt no projection at all. Lets a regression assert rehydration
/// itself rather than a log line.
pub fn rehydrated_refinement_round(
    actor: &crate::actor::CoordinatorActor,
    run_id: &str,
) -> Option<i32> {
    actor
        .active_refinements
        .get(run_id)
        .map(|state| state.current_round)
}

/// Enter the production stalled-outcome path for one materialized role
/// dispatch, exactly as `drive_one_refinement` does when a round's session is
/// gone and its outcome could not be applied.
///
/// This exists because the in-memory session projection the loop keys that
/// path on is deliberately not persisted across a restart; a server-layer
/// regression therefore cannot reach the path through a boot alone.
pub async fn apply_stalled_refinement_outcome(
    actor: &mut crate::actor::CoordinatorActor,
    fixture: &StartupRefinementFixture,
) {
    use crate::refinement::RefinementPhase;
    use crate::refinement_dispatch::RefinementSession;
    use crate::refinement_outcome::RefinementOutcomeApplication;

    let session = RefinementSession {
        run_id: fixture.run_id.clone(),
        generation: fixture.generation,
        task_id: fixture.task_id.clone(),
        phase: RefinementPhase::AdversaryAttack,
        dispatched_at: std::time::Instant::now(),
        session_started_at: Some(std::time::Instant::now()),
        model_id: "test/mock".to_owned(),
    };
    actor
        .handle_stalled_outcome_application(
            &fixture.run_id.clone(),
            &session,
            RefinementOutcomeApplication::Retryable,
        )
        .await;
}

/// Close a materialized refinement role task through the production
/// `close_refinement_task` entry point — what the loop does when a round's
/// task finishes. This is the durable state the stalled-outcome retry ledger
/// is keyed on.
pub async fn close_refinement_role_task(
    actor: &mut crate::actor::CoordinatorActor,
    fixture: &StartupRefinementFixture,
) {
    actor
        .close_refinement_task(
            &fixture.task_id,
            "startup refinement fixture round complete",
        )
        .await;
}

// ── Resident-admission seam for the out-of-crate conformance target ───────
//
// `CoordinatorActor::resident_admission_allows`, `model_under_user_cap` and
// `lane_under_user_cap` are `pub(crate)` and stay that way: nothing outside
// this crate may call the dispatch admission primitives in production. The
// `model_admission_conformance` integration target lives outside the crate, so
// it reaches them through these forwarders, which are compiled only under
// `cfg(test)` or the `test-support` feature.
//
// Each forwarder has no logic of its own — it calls exactly the function the
// production dispatch path calls, with the same arguments in the same order.
// A regression in the primitive is therefore visible through the forwarder.

/// Forward to the production resident-admission conjunction applied at the
/// outer dispatch boundary (`CoordinatorActor::resident_admission_allows`,
/// called from `dispatch::task_dispatch`'s multi-model candidate filter).
pub fn resident_admission_allows(
    running_by_model: &std::collections::HashMap<(String, String), u32>,
    running_by_lane: &std::collections::HashMap<(String, djinn_core::models::ModelLane), u32>,
    user: &str,
    model: &str,
    role: &str,
    max_sessions: &std::collections::HashMap<String, u32>,
    lane_max_sessions: Option<&djinn_core::models::LaneMaxSessions>,
) -> bool {
    crate::actor::CoordinatorActor::resident_admission_allows(
        running_by_model,
        running_by_lane,
        user,
        model,
        role,
        max_sessions,
        lane_max_sessions,
    )
}

/// Forward to the shared per-user/per-model cap primitive.
pub fn model_under_user_cap(
    running_by_user_model: &std::collections::HashMap<(String, String), u32>,
    creator: &str,
    model: &str,
    cap: u32,
) -> bool {
    crate::dispatch::model_under_user_cap(running_by_user_model, creator, model, cap)
}

/// Forward to the shared per-user/per-lane cap primitive.
pub fn lane_under_user_cap(
    running_by_user_lane: &std::collections::HashMap<(String, djinn_core::models::ModelLane), u32>,
    creator: &str,
    lane: djinn_core::models::ModelLane,
    cap: Option<u32>,
) -> bool {
    crate::dispatch::lane_under_user_cap(running_by_user_lane, creator, lane, cap)
}

// ── Ready-dispatch seam for the out-of-crate conformance target ───────────

/// Drive one production ready-dispatch pass.
///
/// Forwards to `CoordinatorActor::dispatch_ready_tasks` with no logic of its
/// own — the same call `CoordinatorActor::run` makes on every tick. Awaiting it
/// gives the caller a happens-before edge on a *finished* pass, so an assertion
/// afterwards observes the pass's durable effects rather than a race.
pub async fn run_dispatch_ready_tasks(
    actor: &mut crate::actor::CoordinatorActor,
    project_filter: Option<&str>,
) {
    actor.dispatch_ready_tasks(project_filter).await;
}

/// The number of dispatches the actor has performed since it was built.
///
/// This is the actor's own counter, incremented on the dispatch path — not a
/// count the caller supplied.
#[must_use]
pub fn dispatched_count(actor: &crate::actor::CoordinatorActor) -> u64 {
    actor.dispatched
}

/// Open the dispatch breaker for one task, or clear it.
///
/// `dispatch_cooldowns` is the exact map `dispatch_ready_tasks` consults
/// before it looks at anything else about a candidate — the production backoff
/// path writes it and the ready pass reads it. This forwarder writes the same
/// entry so an out-of-crate conformance scenario can put a real task behind a
/// real open breaker without a second cooldown mechanism of its own.
pub fn set_dispatch_cooldown_for_test(
    actor: &mut crate::actor::CoordinatorActor,
    task_id: &str,
    remaining: Option<std::time::Duration>,
) {
    match remaining {
        Some(remaining) => {
            actor
                .dispatch_cooldowns
                .insert(task_id.to_owned(), std::time::Instant::now() + remaining);
        }
        None => {
            actor.dispatch_cooldowns.remove(task_id);
        }
    }
}
