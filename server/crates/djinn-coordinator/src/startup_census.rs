//! Immutable, pre-mutation cluster evidence collected during startup recovery.
//!
//! A successful LIST is the primary snapshot.  An omitted deterministic Job is
//! deliberately not called absent until a separate GET says so; LIST/GET failures
//! remain unknown.  This module is shared by all startup reapers so later stages
//! do not accidentally re-query a world already changed by an earlier stage.

use std::collections::HashMap;
use std::sync::Arc;

use djinn_core::models::{TaskRunRecord, TaskRunStatus};
use djinn_db::{Database, TaskRunRepository};
use djinn_k8s::{ObjectPresence, WorkloadInventory, WorkloadObjectKind};

/// One successful namespace LIST, retaining each Job's terminal condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterJobListing {
    Listed(HashMap<String, bool>),
    Unavailable,
    /// Inventory absence is a deliberately supported legacy mode, not a failed
    /// or unanswered observation.
    NotConfigured,
}

/// Why an observation authorizes destructive recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoneProvenance {
    /// A successful LIST omitted the identity and an independent GET returned
    /// `Absent`.
    AuthoritativelyAbsent,
    /// The successful LIST observed the identity with a terminal condition.
    TerminalPresent,
}

/// Immutable evidence for one durable task-run identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunWitness {
    Live,
    Gone(GoneProvenance),
    Unknown,
}

/// Durable state captured before any startup mutation.  The `Unknown` arm is
/// intentionally retained for corrupt/future status values rather than treating
/// an unfamiliar row as terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableRunState {
    Starting,
    Running,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CensusTaskRun {
    pub task_id: String,
    pub task_run_id: String,
    pub durable_state: DurableRunState,
    pub witness: TaskRunWitness,
}

impl CensusTaskRun {
    /// Whether this run's immutable evidence authorizes a destructive startup
    /// mutation of the run itself or of anything linked to it.
    ///
    /// A durable `starting` row whose Job is authoritatively absent can still
    /// be inside the commit-then-CREATE window, so authoritative absence alone
    /// never authorizes destruction for it. Every startup stage shares this
    /// single rule: Stage A's session interruption, Stage B's task-run reaping,
    /// and (through [`TaskCensusProjection`]) Stage C's attempt classification
    /// must not disagree about whether one identity is destructively gone.
    pub fn destructive_mutation_authorized(&self) -> bool {
        matches!(
            (self.durable_state, self.witness),
            (_, TaskRunWitness::Gone(GoneProvenance::TerminalPresent))
                | (
                    DurableRunState::Running,
                    TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)
                )
        )
    }
}

/// Per-task reduction used by stages which classify attempts rather than a
/// single run.  A starting row with authoritative absence can still be between
/// committing its ledger row and CREATE, so it is fenced as `CreationTransit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskCensusProjection {
    Live,
    CreationTransit,
    Unknown,
    DestructivelyGone,
}

/// The inventory result remains explicit even when there are no durable runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryAvailability {
    NotConfigured,
    Available,
    Unavailable,
}

/// The single startup snapshot.  It contains no mutable references and is safe
/// to hand from Stage A into subsequent coordinator recovery stages.
#[derive(Clone, Debug)]
pub struct StartupCensus {
    availability: InventoryAvailability,
    listing: ClusterJobListing,
    runs: Vec<CensusTaskRun>,
    task_projections: HashMap<String, TaskCensusProjection>,
}

impl StartupCensus {
    /// Acquire all evidence before a startup reaper changes durable state.
    /// Exactly one `list` is made when inventory is configured.
    pub async fn acquire(
        db: Database,
        inventory: Option<Arc<dyn WorkloadInventory>>,
    ) -> Result<Self, djinn_db::Error> {
        let durable_runs = TaskRunRepository::new(db).list_startup_live().await?;
        let listing = match inventory.as_ref() {
            None => ClusterJobListing::NotConfigured,
            Some(inventory) => match inventory.list().await {
                Ok(records) => ClusterJobListing::Listed(
                    records
                        .into_iter()
                        .filter(|record| record.kind == WorkloadObjectKind::Job)
                        .map(|record| (record.name, record.terminal))
                        .collect(),
                ),
                Err(error) => {
                    tracing::warn!(%error, "startup census workload LIST failed; preserving unknown evidence");
                    ClusterJobListing::Unavailable
                }
            },
        };

        let availability = match listing {
            ClusterJobListing::NotConfigured => InventoryAvailability::NotConfigured,
            ClusterJobListing::Unavailable => InventoryAvailability::Unavailable,
            ClusterJobListing::Listed(_) => InventoryAvailability::Available,
        };
        let mut runs = Vec::with_capacity(durable_runs.len());
        for run in durable_runs {
            let durable_state = durable_run_state(&run);
            let witness = witness_for_run(&run.id, &listing, inventory.as_deref()).await;
            runs.push(CensusTaskRun {
                task_id: run.task_id,
                task_run_id: run.id,
                durable_state,
                witness,
            });
        }
        let task_projections = project_tasks(&runs);
        Ok(Self {
            availability,
            listing,
            runs,
            task_projections,
        })
    }

    pub fn availability(&self) -> InventoryAvailability {
        self.availability
    }
    pub fn listing(&self) -> &ClusterJobListing {
        &self.listing
    }
    pub fn runs(&self) -> &[CensusTaskRun] {
        &self.runs
    }
    pub fn task_projection(&self, task_id: &str) -> Option<TaskCensusProjection> {
        self.task_projections.get(task_id).copied()
    }
}

fn durable_run_state(run: &TaskRunRecord) -> DurableRunState {
    match run.status.parse::<TaskRunStatus>() {
        Ok(TaskRunStatus::Starting) => DurableRunState::Starting,
        Ok(TaskRunStatus::Running) => DurableRunState::Running,
        _ => DurableRunState::Unknown,
    }
}

async fn witness_for_run(
    task_run_id: &str,
    listing: &ClusterJobListing,
    inventory: Option<&dyn WorkloadInventory>,
) -> TaskRunWitness {
    let ClusterJobListing::Listed(listed) = listing else {
        return TaskRunWitness::Unknown;
    };
    let job_name = djinn_k8s::taskrun_job_name(task_run_id);
    if let Some(&terminal) = listed.get(&job_name) {
        return if terminal {
            TaskRunWitness::Gone(GoneProvenance::TerminalPresent)
        } else {
            TaskRunWitness::Live
        };
    }
    let Some(inventory) = inventory else {
        return TaskRunWitness::Unknown;
    };
    match inventory.presence(WorkloadObjectKind::Job, &job_name).await {
        ObjectPresence::Absent => TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
        ObjectPresence::Present { .. } => TaskRunWitness::Live,
        ObjectPresence::Uncertain => TaskRunWitness::Unknown,
    }
}

fn project_tasks(runs: &[CensusTaskRun]) -> HashMap<String, TaskCensusProjection> {
    let mut projections = HashMap::new();
    for run in runs {
        let next = match run.witness {
            TaskRunWitness::Live => TaskCensusProjection::Live,
            TaskRunWitness::Unknown => TaskCensusProjection::Unknown,
            TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)
                if run.durable_state == DurableRunState::Starting =>
            {
                TaskCensusProjection::CreationTransit
            }
            TaskRunWitness::Gone(_) => TaskCensusProjection::DestructivelyGone,
        };
        projections
            .entry(run.task_id.clone())
            .and_modify(|current| *current = combine_projection(*current, next))
            .or_insert(next);
    }
    projections
}

fn combine_projection(
    current: TaskCensusProjection,
    next: TaskCensusProjection,
) -> TaskCensusProjection {
    use TaskCensusProjection::*;
    match (current, next) {
        (Live, _) | (_, Live) => Live,
        (Unknown, _) | (_, Unknown) => Unknown,
        (CreationTransit, _) | (_, CreationTransit) => CreationTransit,
        (DestructivelyGone, DestructivelyGone) => DestructivelyGone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use djinn_core::events::EventBus;
    use djinn_db::{
        CreateTaskAttemptParams, CreateTaskRunParams, TaskAttemptRepository, TaskRepository,
    };
    use djinn_k8s::UidGetResult;

    /// Controllable namespace inventory that records the concrete acquisition
    /// calls. These tests deliberately enter through `StartupCensus::acquire`.
    struct CountingInventory {
        list_result: Result<Vec<djinn_k8s::WorkloadRecord>, String>,
        presence_result: ObjectPresence,
        list_calls: AtomicUsize,
        presence_calls: AtomicUsize,
        presence_names: Mutex<Vec<String>>,
    }

    impl CountingInventory {
        fn listed_empty(presence_result: ObjectPresence) -> Self {
            Self {
                list_result: Ok(Vec::new()),
                presence_result,
                list_calls: AtomicUsize::new(0),
                presence_calls: AtomicUsize::new(0),
                presence_names: Mutex::new(Vec::new()),
            }
        }

        fn list_fails() -> Self {
            Self {
                list_result: Err("apiserver unavailable".to_owned()),
                presence_result: ObjectPresence::Absent,
                list_calls: AtomicUsize::new(0),
                presence_calls: AtomicUsize::new(0),
                presence_names: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkloadInventory for CountingInventory {
        async fn list(&self) -> Result<Vec<djinn_k8s::WorkloadRecord>, String> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            self.list_result.clone()
        }

        async fn get_uid(
            &self,
            _kind: WorkloadObjectKind,
            _name: &str,
            _uid: &str,
        ) -> UidGetResult {
            UidGetResult::Uncertain
        }

        async fn presence(&self, _kind: WorkloadObjectKind, name: &str) -> ObjectPresence {
            self.presence_calls.fetch_add(1, Ordering::SeqCst);
            self.presence_names
                .lock()
                .expect("presence names mutex")
                .push(name.to_owned());
            self.presence_result.clone()
        }
    }

    /// Use a real task-run repository row: acquisition must read the durable
    /// startup ledger, not merely accept an in-memory `CensusTaskRun` fixture.
    async fn seed_running_run() -> (Database, String, String) {
        let db = crate::test_helpers::create_test_db();
        let project = crate::test_helpers::create_test_project(&db).await;
        let task = TaskRepository::new(db.clone(), EventBus::noop())
            .create_fixture_in_project(
                &project.id,
                None,
                "startup census acquisition",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                None,
            )
            .await
            .expect("create task fixture");
        let run_id = uuid::Uuid::now_v7().to_string();
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: "manual",
                status: Some("running"),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create running task-run fixture");
        (db, task.id, run_id)
    }

    async fn seed_old_pending_attempt(db: &Database, task_id: &str) -> String {
        let attempt_id = uuid::Uuid::now_v7().to_string();
        TaskAttemptRepository::new(db.clone())
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &attempt_id,
                task_id,
                role: "worker",
                dispatch_key: &format!("{task_id}:worker:{attempt_id}"),
                session_id: None,
                attempt_seq: None,
                dispatch_owner_incarnation_id: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create pending attempt fixture");
        djinn_db::test_support::backdate_task_attempt_created_at(db, &attempt_id, "30 seconds")
            .await;
        attempt_id
    }

    async fn seed_task(db: &Database, project_id: &str, title: &str) -> String {
        TaskRepository::new(db.clone(), EventBus::noop())
            .create_fixture_in_project(
                project_id,
                None,
                title,
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                None,
            )
            .await
            .expect("create task fixture")
            .id
    }

    async fn seed_run(db: &Database, project_id: &str, task_id: &str, status: &str) -> String {
        let run_id = uuid::Uuid::now_v7().to_string();
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id,
                task_id,
                trigger_type: "manual",
                status: Some(status),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create task-run fixture");
        run_id
    }

    fn census_from_runs(runs: Vec<CensusTaskRun>) -> StartupCensus {
        let task_projections = project_tasks(&runs);
        StartupCensus {
            availability: InventoryAvailability::Available,
            listing: ClusterJobListing::Listed(HashMap::new()),
            runs,
            task_projections,
        }
    }

    async fn attempt_outcome(db: &Database, attempt_id: &str) -> String {
        TaskAttemptRepository::new(db.clone())
            .get(attempt_id)
            .await
            .expect("read attempt")
            .expect("attempt exists")
            .outcome
    }

    #[tokio::test]
    async fn acquisition_confirms_omitted_run_absent_once() {
        let (db, task_id, run_id) = seed_running_run().await;
        let inventory = Arc::new(CountingInventory::listed_empty(ObjectPresence::Absent));

        let census = StartupCensus::acquire(db, Some(inventory.clone()))
            .await
            .expect("acquire startup census");

        assert_eq!(inventory.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(inventory.presence_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            inventory
                .presence_names
                .lock()
                .expect("presence names mutex")
                .as_slice(),
            [djinn_k8s::taskrun_job_name(&run_id)]
        );
        assert_eq!(census.availability(), InventoryAvailability::Available);
        assert_eq!(
            census.runs()[0].witness,
            TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)
        );
        assert_eq!(
            census.task_projection(&task_id),
            Some(TaskCensusProjection::DestructivelyGone)
        );
    }

    #[tokio::test]
    async fn acquisition_list_failure_is_unknown_without_get() {
        let (db, task_id, _) = seed_running_run().await;
        let inventory = Arc::new(CountingInventory::list_fails());

        let census = StartupCensus::acquire(db, Some(inventory.clone()))
            .await
            .expect("acquire startup census");

        assert_eq!(inventory.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(inventory.presence_calls.load(Ordering::SeqCst), 0);
        assert_eq!(census.availability(), InventoryAvailability::Unavailable);
        assert_eq!(census.runs()[0].witness, TaskRunWitness::Unknown);
        assert_eq!(
            census.task_projection(&task_id),
            Some(TaskCensusProjection::Unknown)
        );
    }

    #[tokio::test]
    async fn acquisition_uncertain_get_is_unknown_after_one_list() {
        let (db, task_id, _) = seed_running_run().await;
        let inventory = Arc::new(CountingInventory::listed_empty(ObjectPresence::Uncertain));

        let census = StartupCensus::acquire(db, Some(inventory.clone()))
            .await
            .expect("acquire startup census");

        assert_eq!(inventory.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(inventory.presence_calls.load(Ordering::SeqCst), 1);
        assert_eq!(census.availability(), InventoryAvailability::Available);
        assert_eq!(census.runs()[0].witness, TaskRunWitness::Unknown);
        assert_eq!(
            census.task_projection(&task_id),
            Some(TaskCensusProjection::Unknown)
        );
    }

    #[test]
    fn projection_fences_starting_authoritative_absence() {
        let runs = vec![CensusTaskRun {
            task_id: "task".into(),
            task_run_id: "run".into(),
            durable_state: DurableRunState::Starting,
            witness: TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
        }];
        assert_eq!(
            project_tasks(&runs).get("task"),
            Some(&TaskCensusProjection::CreationTransit)
        );
    }

    #[test]
    fn projection_preserves_unknown_over_destructive_evidence() {
        let runs = vec![
            CensusTaskRun {
                task_id: "task".into(),
                task_run_id: "gone".into(),
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Gone(GoneProvenance::TerminalPresent),
            },
            CensusTaskRun {
                task_id: "task".into(),
                task_run_id: "unknown".into(),
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Unknown,
            },
        ];
        assert_eq!(
            project_tasks(&runs).get("task"),
            Some(&TaskCensusProjection::Unknown)
        );
    }

    #[test]
    fn projection_preserves_live_and_creation_transit_over_destructive_evidence() {
        let gone = CensusTaskRun {
            task_id: "task".into(),
            task_run_id: "gone".into(),
            durable_state: DurableRunState::Running,
            witness: TaskRunWitness::Gone(GoneProvenance::TerminalPresent),
        };
        let live = CensusTaskRun {
            task_id: "task".into(),
            task_run_id: "live".into(),
            durable_state: DurableRunState::Running,
            witness: TaskRunWitness::Live,
        };
        let transit = CensusTaskRun {
            task_id: "task".into(),
            task_run_id: "starting".into(),
            durable_state: DurableRunState::Starting,
            witness: TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
        };
        assert_eq!(
            project_tasks(&[gone.clone(), live]).get("task"),
            Some(&TaskCensusProjection::Live)
        );
        assert_eq!(
            project_tasks(&[gone, transit]).get("task"),
            Some(&TaskCensusProjection::CreationTransit)
        );
    }

    /// Database-backed Stage C matrix using real task/run/attempt identities and
    /// unreduced mixed-run census evidence, rather than an authorization helper.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn startup_census_stage_c_projection_reduction_matrix() {
        let db = crate::test_helpers::create_test_db();
        let project = crate::test_helpers::create_test_project(&db).await;
        let live_task = seed_task(&db, &project.id, "stage c gone plus live").await;
        let transit_task = seed_task(&db, &project.id, "stage c gone plus transit").await;
        let unknown_task = seed_task(&db, &project.id, "stage c gone plus unknown").await;
        let all_gone_task = seed_task(&db, &project.id, "stage c all gone").await;

        let live_attempt = seed_old_pending_attempt(&db, &live_task).await;
        let transit_attempt = seed_old_pending_attempt(&db, &transit_task).await;
        let unknown_attempt = seed_old_pending_attempt(&db, &unknown_task).await;
        let all_gone_attempt = seed_old_pending_attempt(&db, &all_gone_task).await;

        let runs = vec![
            CensusTaskRun {
                task_id: live_task.clone(),
                task_run_id: seed_run(&db, &project.id, &live_task, "running").await,
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Gone(GoneProvenance::TerminalPresent),
            },
            CensusTaskRun {
                task_id: live_task.clone(),
                task_run_id: seed_run(&db, &project.id, &live_task, "running").await,
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Live,
            },
            CensusTaskRun {
                task_id: transit_task.clone(),
                task_run_id: seed_run(&db, &project.id, &transit_task, "running").await,
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Gone(GoneProvenance::TerminalPresent),
            },
            CensusTaskRun {
                task_id: transit_task.clone(),
                task_run_id: seed_run(&db, &project.id, &transit_task, "starting").await,
                durable_state: DurableRunState::Starting,
                witness: TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
            },
            CensusTaskRun {
                task_id: unknown_task.clone(),
                task_run_id: seed_run(&db, &project.id, &unknown_task, "running").await,
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
            },
            CensusTaskRun {
                task_id: unknown_task.clone(),
                task_run_id: seed_run(&db, &project.id, &unknown_task, "running").await,
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Unknown,
            },
            CensusTaskRun {
                task_id: all_gone_task.clone(),
                task_run_id: seed_run(&db, &project.id, &all_gone_task, "running").await,
                durable_state: DurableRunState::Running,
                witness: TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
            },
            CensusTaskRun {
                task_id: all_gone_task.clone(),
                task_run_id: seed_run(&db, &project.id, &all_gone_task, "starting").await,
                durable_state: DurableRunState::Starting,
                witness: TaskRunWitness::Gone(GoneProvenance::TerminalPresent),
            },
        ];

        let census = census_from_runs(runs);
        assert_eq!(
            census.task_projection(&live_task),
            Some(TaskCensusProjection::Live)
        );
        assert_eq!(
            census.task_projection(&transit_task),
            Some(TaskCensusProjection::CreationTransit)
        );
        assert_eq!(
            census.task_projection(&unknown_task),
            Some(TaskCensusProjection::Unknown)
        );
        assert_eq!(
            census.task_projection(&all_gone_task),
            Some(TaskCensusProjection::DestructivelyGone)
        );

        crate::health::reap_orphaned_pending_attempts_for_startup_with_census(
            &db,
            &uuid::Uuid::now_v7().to_string(),
            &census,
        )
        .await;

        assert_eq!(attempt_outcome(&db, &live_attempt).await, "pending");
        assert_eq!(attempt_outcome(&db, &transit_attempt).await, "pending");
        assert_eq!(attempt_outcome(&db, &unknown_attempt).await, "pending");
        assert_eq!(attempt_outcome(&db, &all_gone_attempt).await, "interrupted");
        assert!(logs_contain("stage=\"startup_stage_c\""));
        assert!(logs_contain("reason=\"unknown\""));
        assert!(logs_contain(&unknown_task));
    }

    #[tokio::test]
    async fn configured_reapers_consume_the_acquired_census_without_inventory_calls() {
        let (db, task_id, run_id) = seed_running_run().await;
        let attempt_id = seed_old_pending_attempt(&db, &task_id).await;
        let inventory = Arc::new(CountingInventory::listed_empty(ObjectPresence::Absent));
        let census = StartupCensus::acquire(db.clone(), Some(inventory.clone()))
            .await
            .expect("acquire startup census");
        let list_calls = inventory.list_calls.load(Ordering::SeqCst);
        let presence_calls = inventory.presence_calls.load(Ordering::SeqCst);
        crate::health::reap_stale_task_runs_for_startup_with_census(&db, &census).await;

        // This live run did not exist when the census was acquired. Switching
        // Stage C back to `list_orphaned_pending` would suppress the attempt by
        // re-reading this post-census task-run state.
        let project_id = TaskRunRepository::new(db.clone())
            .get(&run_id)
            .await
            .expect("read census run")
            .expect("census run exists")
            .project_id;
        let post_census_run = seed_run(&db, &project_id, &task_id, "running").await;
        crate::health::reap_orphaned_pending_attempts_for_startup_with_census(
            &db,
            &uuid::Uuid::now_v7().to_string(),
            &census,
        )
        .await;
        assert_eq!(inventory.list_calls.load(Ordering::SeqCst), list_calls);
        assert_eq!(
            inventory.presence_calls.load(Ordering::SeqCst),
            presence_calls
        );
        assert_eq!(
            TaskRunRepository::new(db.clone())
                .get(&run_id)
                .await
                .expect("read task run")
                .expect("task run exists")
                .status,
            "interrupted"
        );
        assert_eq!(
            TaskRunRepository::new(db.clone())
                .get(&post_census_run)
                .await
                .expect("read post-census run")
                .expect("post-census run exists")
                .status,
            "running"
        );
        assert_eq!(
            TaskAttemptRepository::new(db.clone())
                .get(&attempt_id)
                .await
                .expect("read attempt")
                .expect("attempt exists")
                .outcome,
            "interrupted"
        );
    }
}
