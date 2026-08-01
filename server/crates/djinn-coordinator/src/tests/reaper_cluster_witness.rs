//! The stuck-task reaper must not kill live sessions after a restart.
//!
//! # The incident these tests encode
//!
//! 2026-08-01, rolling deploy of v0.7.31. The coordinator's stuck-task scan
//! released five LIVE tasks: three workers `in_progress → open` at 00:09:34 and
//! two reviewers `in_task_review → needs_task_review` at 00:10:33. Task `zd7p`'s
//! reviewer submitted an `approved` verdict 31 seconds AFTER its demotion; the
//! verdict row persisted and the state transition was dropped, so the task was
//! re-reviewed from scratch ~12h later at ~570k tokens of waste.
//!
//! The reaper's gate was `SlotPoolHandle::has_session` — an IN-MEMORY map a
//! restart wipes. Every live session read as absent. The liveness classifier
//! consulted below it could not help: `build_liveness_evidence` derives
//! `pod_phase` from the SAME in-memory map, so on that path it is always
//! `PodPhase::Absent` + `ActivitySignal::Idle` and `Verdict::Alive` is
//! structurally unreachable.
//!
//! # What every test here asserts
//!
//! The SIDE EFFECT, never a returned enum or a log line: the task's `status`
//! column in the database, and whether the `sessions` row is still `running`.
//! A test that passed while the reaper still ran would be worthless.
//!
//! Each test's doc comment names the mutation that turns it red.

use super::*;
use djinn_db::{CreateSessionParams, CreateTaskRunParams, SessionRepository, TaskRunRepository};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
    taskrun_job_name,
};

/// A namespace holding exactly the Jobs it was given, answering `presence` from
/// the same set a live API server would.
///
/// Modelled on `build_lease_reclaim_tests::NamespaceInventory`, which is the
/// module this fix follows.
struct NamespaceInventory {
    records: Vec<WorkloadRecord>,
    /// The LIST itself fails — a degraded or unreachable API server. This is
    /// the case that must never read as "every object is absent".
    list_fails: bool,
    /// The LIST succeeds but a direct GET cannot answer. `Uncertain` is proof of
    /// nothing.
    probe_uncertain: bool,
}

impl NamespaceInventory {
    fn empty() -> Self {
        Self {
            records: Vec::new(),
            list_fails: false,
            probe_uncertain: false,
        }
    }

    /// A namespace holding one task-run Job for `task_run_id`. `terminal` is the
    /// Job's `Complete`/`Failed` condition — the difference between "the pod is
    /// working" and "the Job is waiting out its `ttlSecondsAfterFinished`".
    fn holding_taskrun(task_run_id: &str, terminal: bool) -> Self {
        Self {
            records: vec![WorkloadRecord {
                kind: WorkloadObjectKind::Job,
                name: taskrun_job_name(task_run_id),
                uid: Some(format!("uid-{task_run_id}")),
                labels: Default::default(),
                terminal,
                images: Vec::new(),
                commands: Vec::new(),
            }],
            list_fails: false,
            probe_uncertain: false,
        }
    }

    fn unlistable() -> Self {
        Self {
            records: Vec::new(),
            list_fails: true,
            probe_uncertain: false,
        }
    }

    fn unprobeable() -> Self {
        Self {
            records: Vec::new(),
            list_fails: false,
            probe_uncertain: true,
        }
    }
}

#[async_trait::async_trait]
impl WorkloadInventory for NamespaceInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        if self.list_fails {
            return Err("apiserver 503: the namespace could not be listed".to_owned());
        }
        Ok(self.records.clone())
    }

    async fn get_uid(&self, _kind: WorkloadObjectKind, name: &str, _uid: &str) -> UidGetResult {
        if self.probe_uncertain {
            return UidGetResult::Uncertain;
        }
        if self.records.iter().any(|record| record.name == name) {
            UidGetResult::Present
        } else {
            UidGetResult::NotFound
        }
    }

    async fn presence(&self, _kind: WorkloadObjectKind, name: &str) -> ObjectPresence {
        if self.probe_uncertain {
            return ObjectPresence::Uncertain;
        }
        match self.records.iter().find(|record| record.name == name) {
            Some(record) => ObjectPresence::Present {
                uid: record.uid.clone(),
            },
            None => ObjectPresence::Absent,
        }
    }
}

/// One task sitting in `status` with a live `task_runs` row and a `running`
/// session bound to it — the exact durable shape the five reaped tasks had at
/// 00:09:34, with the in-memory pool empty because the coordinator had just
/// restarted.
struct LiveTask {
    task: djinn_core::models::Task,
    task_run_id: String,
    session_id: String,
}

async fn seed_live_task(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    title: &str,
    status: &str,
    role: &str,
) -> LiveTask {
    let (task, _note) = create_task_with_note(db, tx, title).await;
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    task_repo.set_status(&task.id, status).await.unwrap();

    let task_run_id = format!("019ea3bd-a305-73e3-806c-{:012x}", rand_suffix());
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: role,
            metadata_json: None,
            task_run_id: Some(&task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let task = task_repo.get(&task.id).await.unwrap().unwrap();
    LiveTask {
        task,
        task_run_id,
        session_id: session.id,
    }
}

fn rand_suffix() -> u64 {
    // Unique per call within a process; task-run ids only need to be distinct.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Backdate the session so the ready-state path's `session_predates_task_status`
/// precondition holds, which is what makes `open`/`needs_task_review` tasks
/// reach the reap gate at all.
async fn backdate(db: &Database, tx: &broadcast::Sender<DjinnEventEnvelope>, session_id: &str) {
    SessionRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .backdate_started_at(session_id, "20 minutes")
        .await
        .unwrap();
}

async fn task_status(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    task_id: &str,
) -> String {
    TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .get(task_id)
        .await
        .unwrap()
        .unwrap()
        .status
}

async fn session_is_running(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    session_id: &str,
) -> bool {
    SessionRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .list_active()
        .await
        .unwrap()
        .iter()
        .any(|session| session.id == session_id)
}

// ── Execution-state path (`in_progress` / `in_task_review`) ─────────────────

/// **The regression test.** Post-restart coordinator: the in-memory slot pool
/// is EMPTY, and a live (non-terminal) task-run Job exists for the task. The
/// task must stay exactly where it is and its session must stay `running`.
///
/// Mutation that makes this fail: in `witness_for_task_run`, return
/// `ClusterWitness::Gone` instead of `ClusterWitness::Live` for a listed,
/// non-terminal Job — or delete the `if !witness.permits_reap() { continue; }`
/// gate on the execution-state path. Either way the task moves to `open` and
/// the session is interrupted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_job_blocks_execution_state_reap_after_coordinator_restart() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "restart-live-worker", "in_progress", "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    // A restarted coordinator: nothing in the pool remembers this task.
    assert!(
        !actor.pool.has_session(&live.task.id).await.unwrap(),
        "precondition: the in-memory pool must be empty, as it is after a restart"
    );
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::holding_taskrun(
        &live.task_run_id,
        false,
    )));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "in_progress",
        "a task whose task-run Job is live must NOT be released, however empty the \
         in-memory pool is"
    );
    assert!(
        session_is_running(&db, &tx, &live.session_id).await,
        "the live session must NOT be interrupted"
    );
}

/// The `zd7p` shape: a reviewer holding `in_task_review` behind a live Job. The
/// demotion to `needs_task_review` is what silently dropped its `approved`
/// verdict 31 seconds later.
///
/// Mutation: same as above — this test additionally pins that the guard covers
/// the reviewer release arm (`TransitionAction::ReleaseTaskReview`), not just
/// the worker one. Changing the gate to `if task.status == "in_progress" &&
/// !witness.permits_reap()` leaves this test red while the worker test passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_job_blocks_reviewer_demotion_after_coordinator_restart() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(
        &db,
        &tx,
        "restart-live-reviewer",
        "in_task_review",
        "reviewer",
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::holding_taskrun(
        &live.task_run_id,
        false,
    )));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "in_task_review",
        "a reviewer with a live Job must keep the task in review; demoting it drops the \
         verdict it is about to submit"
    );
    assert!(
        session_is_running(&db, &tx, &live.session_id).await,
        "the live reviewer session must NOT be interrupted"
    );
}

/// The fix must not disable recovery. A genuinely dead orphan — pool empty AND
/// no Job anywhere in the namespace — is still released and its ghost session
/// still finalized.
///
/// Mutation: make `ClusterWitness::permits_reap` return `false` for `Gone`, or
/// make `cluster_witness_for_task` return `Unknown` unconditionally. The task
/// stays `in_progress` and this test fails — which is what proves the two live
/// tests above are not passing merely because the reaper stopped working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_orphan_with_no_job_is_still_reaped() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "dead-orphan-worker", "in_progress", "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::empty()));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "open",
        "a task-run with no Job in the namespace is a real orphan and must be released"
    );
    assert!(
        !session_is_running(&db, &tx, &live.session_id).await,
        "the orphaned session row must be finalized"
    );
}

/// Presence is not liveness. A task-run Job carries
/// `ttlSecondsAfterFinished: 3600`, so a finished run stays LISTED for an hour
/// after its worker died. Recovery must not be blocked for that hour.
///
/// Mutation: in `witness_for_task_run`, drop the terminal flag and return
/// `ClusterWitness::Live` for any listed object. The task stays `in_progress`
/// and this fails — the same conflation that stranded three `launching` build
/// leases behind three `Complete` Jobs in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_job_lingering_on_its_ttl_does_not_block_recovery() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "finished-job-worker", "in_progress", "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::holding_taskrun(
        &live.task_run_id,
        true,
    )));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "open",
        "a Job that reached a terminal condition proves its worker is gone, TTL or not"
    );
}

/// A Kubernetes API error must not read as "absent". This is the case that
/// turns one bad minute at the API server into a board-wide massacre.
///
/// Mutation: in `cluster_job_listing`, map the `Err` arm to
/// `ClusterJobListing::Listed(HashMap::new())` instead of
/// `ClusterJobListing::Unavailable`. The task is released and this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_api_server_does_not_reap() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "apiserver-down-worker", "in_progress", "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::unlistable()));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "in_progress",
        "a failed LIST is not evidence that anything is absent"
    );
    assert!(
        session_is_running(&db, &tx, &live.session_id).await,
        "a failed LIST must not interrupt a session"
    );
}

/// The LIST succeeds and does not name the object, but the independent GET
/// cannot answer. `Uncertain` is never proof.
///
/// Mutation: in `witness_for_task_run`, map `ObjectPresence::Uncertain` to
/// `ClusterWitness::Gone` (or drop the `presence` probe entirely and trust the
/// listing's absence). The task is released and this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncertain_absence_probe_does_not_reap() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "uncertain-probe-worker", "in_progress", "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::unprobeable()));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "in_progress",
        "an unanswerable GET is not an absence proof"
    );
    assert!(
        session_is_running(&db, &tx, &live.session_id).await,
        "an unanswerable GET must not interrupt a session"
    );
}

// ── Ready-state path (`open` / `needs_task_review`) ─────────────────────────

/// The same defect shape one branch up: on the ready-state path
/// `interrupt_running_for_task` used to run regardless of any verdict. A stale
/// `open` task whose Job is live must keep its session.
///
/// Mutation: delete the `if !witness.permits_reap() { continue; }` gate on the
/// ready-state path (the one above the `classify_task_liveness` call in the
/// `open`/`needs_task_review` arm). The session is finalized and this fails,
/// while every execution-state test above still passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_job_blocks_ready_state_session_interrupt() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "ready-state-live", "open", "worker").await;
    backdate(&db, &tx, &live.session_id).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::holding_taskrun(
        &live.task_run_id,
        false,
    )));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert!(
        session_is_running(&db, &tx, &live.session_id).await,
        "a ready-state session with a live task-run Job must NOT be finalized"
    );
}

/// The ready-state negative control: with no Job in the namespace the stale
/// session is still finalized, so the guard above has not simply disabled the
/// ready-state orphan sweep.
///
/// Mutation: make `permits_reap` return `false` for `Gone`. This fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_state_orphan_with_no_job_is_still_finalized() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "ready-state-orphan", "open", "worker").await;
    backdate(&db, &tx, &live.session_id).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::empty()));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert!(
        !session_is_running(&db, &tx, &live.session_id).await,
        "a ready-state orphan with no Job must still be finalized"
    );
}

/// The ready-state equivalent of the API-error case.
///
/// Mutation: map `cluster_job_listing`'s `Err` arm to an empty `Listed` map.
/// The session is finalized and this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_state_unreachable_api_server_does_not_interrupt() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "ready-state-apiserver-down", "open", "worker").await;
    backdate(&db, &tx, &live.session_id).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.workload_inventory = Some(Arc::new(NamespaceInventory::unlistable()));

    actor.detect_and_recover_stuck_filtered(None).await;

    assert!(
        session_is_running(&db, &tx, &live.session_id).await,
        "a failed LIST must not finalize a ready-state session either"
    );
}

// ── Invariant guards ───────────────────────────────────────────────────────

/// The fail-safe direction, stated directly: ambiguity sits with liveness, not
/// with absence.
///
/// Mutation: move `Unknown` into the `true` arm of `permits_reap`. This fails,
/// as do both API-error tests and the uncertain-probe test.
#[test]
fn an_unanswered_probe_never_authorizes_a_reap() {
    use crate::dispatch::session_recovery::ClusterWitness;
    assert!(!ClusterWitness::Unknown.permits_reap());
    assert!(!ClusterWitness::Live.permits_reap());
    assert!(ClusterWitness::Gone.permits_reap());
    assert!(ClusterWitness::NotApplicable.permits_reap());
}

/// A deployment with no task-run Jobs at all (dev / in-process runtime, no kube
/// client) keeps its pre-existing behaviour: there is no cluster to corroborate
/// against, so the pool remains the only witness and orphans are still swept.
///
/// Mutation: map `ClusterJobListing::NotConfigured` to `ClusterWitness::Unknown`
/// instead of `NotApplicable`. Stuck-task recovery stops entirely off-server and
/// this fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_with_no_kubernetes_inventory_still_recovers_orphans() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let live = seed_live_task(&db, &tx, "no-inventory-orphan", "in_progress", "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    assert!(
        actor.workload_inventory.is_none(),
        "precondition: the test actor has no Kubernetes inventory"
    );

    actor.detect_and_recover_stuck_filtered(None).await;

    assert_eq!(
        task_status(&db, &tx, &live.task.id).await,
        "open",
        "without a cluster to observe, the reaper keeps its prior behaviour"
    );
}
