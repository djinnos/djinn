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

    pub fn availability(&self) -> InventoryAvailability { self.availability }
    pub fn listing(&self) -> &ClusterJobListing { &self.listing }
    pub fn runs(&self) -> &[CensusTaskRun] { &self.runs }
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
    let Some(inventory) = inventory else { return TaskRunWitness::Unknown; };
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

    #[test]
    fn projection_fences_starting_authoritative_absence() {
        let runs = vec![CensusTaskRun {
            task_id: "task".into(),
            task_run_id: "run".into(),
            durable_state: DurableRunState::Starting,
            witness: TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent),
        }];
        assert_eq!(project_tasks(&runs).get("task"), Some(&TaskCensusProjection::CreationTransit));
    }

    #[test]
    fn projection_preserves_unknown_over_destructive_evidence() {
        let runs = vec![
            CensusTaskRun { task_id: "task".into(), task_run_id: "gone".into(), durable_state: DurableRunState::Running, witness: TaskRunWitness::Gone(GoneProvenance::TerminalPresent) },
            CensusTaskRun { task_id: "task".into(), task_run_id: "unknown".into(), durable_state: DurableRunState::Running, witness: TaskRunWitness::Unknown },
        ];
        assert_eq!(project_tasks(&runs).get("task"), Some(&TaskCensusProjection::Unknown));
    }
}
