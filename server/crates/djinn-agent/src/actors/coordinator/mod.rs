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

use crate::actors::slot::{PoolError, SlotPoolHandle};
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
// These additional imports are only used by `#[cfg(test)]` blocks in child
// submodules (rules, health, prompt_eval, etc.) via `use super::*;`.
#[cfg(test)]
use djinn_core::events::DjinnEventEnvelope;
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
        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
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
            consolidation_runner: Arc::new(consolidation::DbConsolidationRunner::new(db.clone())),
            last_stale_sweep: StdInstant::now(),
            last_auto_dispatch_sweep: StdInstant::now(),
            last_proposal_review_sweep: StdInstant::now(),
            last_graph_refresh: StdInstant::now(),
            graph_warmer: None,
            mirror: None,
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

    // ── Model failover via the health circuit-breaker ────────────────────────

    /// A model tripped on a stall is skipped by `try_dispatch_to_pool`, which
    /// fails over to the next model in the creator's ordered list. This is the
    /// core failover behaviour: without feeding the breaker the first
    /// (preferred) model is always `is_available` and always re-selected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_model_is_skipped_and_dispatch_fails_over_to_next() {
        use std::sync::{Arc, Mutex};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let actor = coordinator_actor_for_tests(&db, &tx);

        let bad = "openai/gpt-5.5".to_string();
        let good = "openai/gpt-5.4".to_string();
        let model_ids = vec![bad.clone(), good.clone()];

        // Trip the preferred model on a zero-token stall.
        actor.health.record_stall(None, &bad);
        assert!(!actor.health.is_available(None, &bad));
        assert!(actor.health.is_available(None, &good));

        // Record which model the dispatch closure is actually invoked with.
        let attempted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let attempted_cl = attempted.clone();
        let outcome = actor
            .try_dispatch_to_pool("failover-test", None, &model_ids, |_pool, model_id| {
                let attempted = attempted_cl.clone();
                let model_id = model_id.to_owned();
                async move {
                    attempted.lock().unwrap().push(model_id);
                    Ok::<(), PoolError>(())
                }
            })
            .await;

        assert!(matches!(outcome, DispatchOutcome::Dispatched));
        let attempted = attempted.lock().unwrap().clone();
        assert_eq!(
            attempted,
            vec![good.clone()],
            "the stalled preferred model must be skipped; dispatch fails over to the next model"
        );
    }

    /// Once the stalled model's cooldown expires it is available again, so the
    /// preferred model is re-selected — the failover self-heals.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_model_recovers_after_cooldown_expires() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let actor = coordinator_actor_for_tests(&db, &tx);

        let bad = "openai/gpt-5.5".to_string();
        actor.health.record_stall(None, &bad);
        assert!(!actor.health.is_available(None, &bad));

        // Simulate cooldown expiry, then a successful run resets the breaker.
        actor.health.enable(None, &bad);
        actor.health.record_success(None, &bad);
        assert!(actor.health.is_available(None, &bad));

        let model_ids = vec![bad.clone()];
        let outcome = actor
            .try_dispatch_to_pool(
                "recover-test",
                None,
                &model_ids,
                |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
            )
            .await;
        assert!(matches!(outcome, DispatchOutcome::Dispatched));
    }

    // ── Zombie-session DB-truth backstop ─────────────────────────────────────

    /// Regression for the xh6f wedge: a session stuck `running` with zero
    /// tokens past the hard cap is reaped purely on DB truth — the row is
    /// finalized and the task released for redispatch — even when the
    /// in-memory fast-path reapers (`stall_killed`, `pool.has_session`) would
    /// skip it. Models a worker that came up, wrote its session row, then died
    /// before producing a token without the slot's `Killed` event ever firing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zombie_zero_token_session_is_reaped_on_db_truth() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, _note) = create_task_with_note(&db, &tx, "zombie-reap").await;

        // Put the task in an execution state, as if dispatched.
        sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
            })
            .await
            .unwrap();
        // Backdate well past the 10-minute hard cap, leaving tokens at 0/0.
        // Match the column's stored format (VARCHAR `YYYY-MM-DDThh:mm:ss.msZ`)
        // so `parse_iso_elapsed` reads it.
        sqlx::query(
            "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
        )
            .bind(&session.id)
            .execute(db.pool())
            .await
            .unwrap();

        assert!(
            session_repo
                .list_active()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == session.id),
            "precondition: zombie session should be listed as running"
        );

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.reap_zombie_sessions().await;

        assert!(
            !session_repo
                .list_active()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == session.id),
            "zombie session row must be finalized by the backstop"
        );
        let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get(&task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, "open",
            "task must be released for redispatch after the zombie is reaped"
        );
        assert!(
            actor.health.is_available(None, "openai/gpt-5.5"),
            "reaping an infra/drift zombie must NOT trip the model breaker: the backstop \
             fires on capacity/OOM/leak/hung-tool conditions, none of which are model \
             evidence — tripping it disables the (often only) model for the scope and \
             turns a transient capacity pinch into a full dispatch outage. Genuine model \
             stalls are owned by the fast-path stall-kill and the supervisor ProviderError path."
        );
    }

    /// A young zero-token session (still inside the fast-path window) is left
    /// alone by the backstop — the 180s stall breaker owns that case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn young_zero_token_session_is_not_reaped() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, _note) = create_task_with_note(&db, &tx, "young-session").await;
        sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
            })
            .await
            .unwrap();

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.reap_zombie_sessions().await;

        assert!(
            session_repo
                .list_active()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == session.id),
            "a session inside the hard-cap window must not be reaped by the backstop"
        );
    }

    /// A zero-token session PAST the hard cap is NOT reaped while its worker
    /// still holds a live RPC connection. This is the K8s false-reap fix: the
    /// in-memory slot/activity bookkeeping can drift for remote pods (making the
    /// activity gate false-negative), but a live connection is ground-truth that
    /// the worker is alive, so the backstop must defer to it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connected_worker_past_hard_cap_is_not_reaped() {
        use djinn_db::{CreateSessionParams, SessionRepository};

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let (task, _note) = create_task_with_note(&db, &tx, "connected-no-reap").await;
        sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

        let run_id = "run-connected-1";
        // `sessions.task_run_id` has an FK to `task_runs`, so seed the run row.
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) VALUES ($1, $2, $3, 'manual', 'running')",
        )
            .bind(run_id)
            .bind(&task.project_id)
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(run_id),
            })
            .await
            .unwrap();
        // Backdate past the 10-minute hard cap, tokens still 0/0.
        sqlx::query(
            "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
        )
            .bind(&session.id)
            .execute(db.pool())
            .await
            .unwrap();

        // Wire a registry that reports a LIVE connection for this run.
        let registry = std::sync::Arc::new(djinn_supervisor::ConnectionRegistry::new());
        registry.register_connected_for_test(run_id).await;
        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor.rpc_registry = Some(registry.clone());

        actor.reap_zombie_sessions().await;

        assert!(
            session_repo
                .list_active()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == session.id),
            "a past-cap session with a live worker connection must NOT be reaped"
        );
        let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get(&task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, "in_progress",
            "task with a live connected worker must stay in_progress, not be released"
        );

        // Sanity: once the connection drops, the same session IS reaped.
        registry.deregister(run_id).await;
        actor.reap_zombie_sessions().await;
        assert!(
            !session_repo
                .list_active()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == session.id),
            "after the worker connection drops, the past-cap zombie must be reaped"
        );
    }

    /// `stall_killed` is keyed by session id and pruned against `list_active()`:
    /// a leftover entry for a session that is no longer running is dropped, so
    /// it can never linger to mask a redispatched successor session for the
    /// same task (the proximate cause of the xh6f permanent wedge).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stall_killed_prunes_sessions_absent_from_active() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let mut actor = coordinator_actor_for_tests(&db, &tx);

        actor
            .stall_killed
            .insert("019e764f-dead-session".to_string());
        // No sessions are running, so the prune (retain by active session id)
        // must clear the stale entry.
        actor.enforce_session_stall_timeout().await;
        assert!(
            actor.stall_killed.is_empty(),
            "stall_killed entries for sessions absent from list_active() must be pruned"
        );
    }

    async fn create_simple_task(
        db: &Database,
        tx: &broadcast::Sender<DjinnEventEnvelope>,
        issue_type: &str,
        title: &str,
    ) -> (djinn_core::models::Task, String) {
        let project = test_helpers::create_test_project(db).await;
        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
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

    // ── Status ───────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_status_is_zero() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let handle = spawn_coordinator(&db, &tx);

        let status = handle.get_status().unwrap();
        assert_eq!(status.tasks_dispatched, 0);
        assert_eq!(status.sessions_recovered, 0);
    }

    // ── Dispatch on open-task event ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_ready_task() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        let outcome = actor
            .try_dispatch_to_pool(
                "T1",
                None,
                &[DEFAULT_MODEL_ID.to_owned()],
                |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
            )
            .await;
        assert!(matches!(outcome, DispatchOutcome::Dispatched));
        actor.dispatched += 1;

        assert!(
            actor.dispatched >= 1,
            "should have dispatched the ready task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trigger_dispatch_increments_counter_for_review_tasks() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        let outcome = actor
            .try_dispatch_to_pool(
                "Review me",
                None,
                &[DEFAULT_MODEL_ID.to_owned()],
                |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
            )
            .await;
        assert!(matches!(outcome, DispatchOutcome::Dispatched));
        actor.dispatched += 1;

        assert!(
            actor.dispatched >= 1,
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
        // Deterministic sync: wait for the coordinator to record the
        // FAILED_CLOSE marker instead of a fixed sleep (which flaked under
        // load — the coordinator actor processes the status_changed event
        // asynchronously and 100ms is not a hard upper bound on latency).
        wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_FAILED_CLOSE, 0).await;

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
        // Deterministic sync on each coordinator-observed side-effect
        // instead of fixed sleeps.  Fixed-duration sleeps flaked because the
        // coordinator actor processes status_changed events asynchronously
        // and 50-150ms is not a hard upper bound on scheduler + DB latency
        // under parallel-test load.
        wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_FAILED_CLOSE, 0).await;
        repo.set_status(&task.id, "open").await.unwrap();
        wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_REOPEN_COUNT, 1).await;
        let reopened_once = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reopened_once.reopen_count, 1);
        let after_first = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!(after_first < 0.5, "first reopen should reduce confidence");

        // Duplicate open→open: the coordinator must treat this as a no-op
        // (marker for reopen_count=1 already exists).  Assert idempotency as
        // an integer invariant — the marker count must not grow — rather
        // than as float-equality on the derived confidence.  Float-equality
        // conflates "penalty not applied" with "penalty not yet applied"
        // and is what made the original test flaky.
        let markers_before_duplicate = outcome_marker_count(&repo, &task.id).await;
        repo.set_status(&task.id, "open").await.unwrap();
        // There is no positive-side-effect marker to poll for a no-op, so
        // drive a follow-up transition that DOES produce a marker and poll
        // for it; once that marker lands we know the duplicate event was
        // drained from the coordinator's queue without producing a
        // second reopen_count=1 marker.
        repo.set_status_with_reason(&task.id, "closed", Some("failed"))
            .await
            .unwrap();
        wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_FAILED_CLOSE, 1).await;
        repo.set_status(&task.id, "open").await.unwrap();
        wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_REOPEN_COUNT, 2).await;

        let reopened_twice = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(reopened_twice.reopen_count, 2);
        let after_second = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
        assert!(
            after_second <= after_first,
            "second reopen should not increase confidence, got after_second={after_second}, after_first={after_first}"
        );
        // Exactly two new markers between the duplicate no-op and now:
        // one FAILED_CLOSE(reopen_count=1) and one REOPEN_COUNT(reopen_count=2).
        // If the duplicate open→open had wrongly applied a penalty, we'd
        // see three.
        let markers_after = outcome_marker_count(&repo, &task.id).await;
        assert_eq!(
            markers_after - markers_before_duplicate,
            2,
            "duplicate open→open must be a no-op: expected +2 markers (FAILED_CLOSE rc=1, REOPEN_COUNT rc=2), got +{}",
            markers_after - markers_before_duplicate,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn below_threshold_does_not_intervene() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let mut actor = coordinator_actor_for_tests(&db, &tx);
        let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD - 1).await;

        let intervened = actor.maybe_intervene_on_stuck_task(&task).await;
        assert!(!intervened, "below threshold must not intervene");

        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        assert!(
            planner_intervention_markers(&repo, &task.id)
                .await
                .is_empty(),
            "no intervention marker should be written below threshold"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_threshold_routes_to_planner_intervention() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let mut actor = coordinator_actor_for_tests(&db, &tx);
        let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;

        let intervened = actor.maybe_intervene_on_stuck_task(&task).await;
        assert!(
            intervened,
            "at threshold must route to planner intervention"
        );

        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Exactly one intervention marker, keyed to the current reopen count.
        let markers = planner_intervention_markers(&repo, &task.id).await;
        assert_eq!(markers.len(), 1, "exactly one intervention marker");
        assert_eq!(markers[0]["reopen_count"], REOPEN_INTERVENTION_THRESHOLD);

        // A Planner review task was created in the same project.
        let reviews = repo.list_by_status("open").await.unwrap();
        assert!(
            reviews
                .iter()
                .any(|t| t.issue_type == "review" && t.project_id == task.project_id),
            "a review (planner intervention) task must be created"
        );

        // The source task carries a PLANNER_ESCALATION comment linking it.
        let comments = repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some("comment".to_string()),
                actor_role: None,
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert!(
            comments
                .iter()
                .any(|c| c.payload.contains("PLANNER_ESCALATION")),
            "source task must record a PLANNER_ESCALATION comment"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intervention_is_idempotent_per_reopen_count() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let mut actor = coordinator_actor_for_tests(&db, &tx);
        let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // First pass intervenes; subsequent passes at the SAME reopen count are
        // suppressed by the marker — no Planner storm while one is in flight.
        assert!(actor.maybe_intervene_on_stuck_task(&task).await);
        assert!(!actor.maybe_intervene_on_stuck_task(&task).await);
        assert!(!actor.maybe_intervene_on_stuck_task(&task).await);

        assert_eq!(
            planner_intervention_markers(&repo, &task.id).await.len(),
            1,
            "idempotent: a single marker for one reopen-count value"
        );

        // A genuine new reopen (count bumps past threshold again) re-arms one
        // fresh intervention.
        repo.set_status(&task.id, "closed").await.unwrap();
        let bumped = repo.set_status(&task.id, "open").await.unwrap();
        assert_eq!(bumped.reopen_count, REOPEN_INTERVENTION_THRESHOLD + 1);

        assert!(
            actor.maybe_intervene_on_stuck_task(&bumped).await,
            "a higher reopen count must re-arm intervention"
        );
        assert_eq!(
            planner_intervention_markers(&repo, &task.id).await.len(),
            2,
            "one marker per distinct reopen-count value"
        );
    }

    /// Second strike: once the Planner has already intervened
    /// (`intervention_count >= MAX_PLANNER_INTERVENTIONS`) and the task has
    /// STILL climbed back to the reopen threshold, the coordinator parks it
    /// terminally instead of escalating to the Planner again — no new marker,
    /// no new review task, and the task ends up `closed`. This is the loop
    /// breaker for the txr4 case (rescope didn't help → stop hogging the slot).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_strike_parks_task_after_prior_intervention() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let mut actor = coordinator_actor_for_tests(&db, &tx);
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

        // Reach the threshold once, simulate a completed planner intervention
        // (bumps intervention_count, resets reopen_count), then climb back to the
        // threshold a second time.
        let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
        repo.reset_intervention_counters(&task.id).await.unwrap();
        for _ in 0..REOPEN_INTERVENTION_THRESHOLD {
            repo.set_status(&task.id, "closed").await.unwrap();
            repo.set_status(&task.id, "open").await.unwrap();
        }
        let task = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(task.intervention_count, 1, "one prior planner intervention");
        assert_eq!(task.reopen_count, REOPEN_INTERVENTION_THRESHOLD);

        let handled = actor.maybe_intervene_on_stuck_task(&task).await;
        assert!(
            handled,
            "second strike must handle the task (caller skips worker dispatch)"
        );

        // Parked terminally — task is closed.
        let parked = repo.get(&task.id).await.unwrap().unwrap();
        assert_eq!(parked.status, "closed", "second strike force-closes the task");

        // No planner intervention marker for this reopen count, and no new
        // planner review task — the loop is broken, not re-escalated.
        assert!(
            !planner_intervention_markers(&repo, &task.id)
                .await
                .iter()
                .any(|m| m["reopen_count"] == REOPEN_INTERVENTION_THRESHOLD),
            "second strike must not write a new planner intervention marker"
        );
        let reviews = repo.list_by_status("open").await.unwrap();
        assert!(
            !reviews
                .iter()
                .any(|t| t.issue_type == "review" && t.project_id == parked.project_id),
            "second strike must not create another planner review task"
        );
    }
}
