//! Coordinator-owned durable admission policy for build-producing workloads.
//!
//! The journal supplies serialization and lifecycle fencing; this module fixes
//! workload classification before dispatch and translates controller facts into
//! the data-only graph-warmer protocol.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionJournalRepository, AdmissionWorkloadKind,
    CreateStartedInput, ReserveAdmissionInput, ReserveAdmissionResult, TerminalAdmissionInput,
    UidFencedAdmissionInput,
};
use djinn_k8s::{
    WarmAdmission, WarmAdmissionError, WarmAdmissionPermit, WarmAdmissionRequest,
    WarmAdmissionTransition,
};
use tokio::sync::{Mutex, Notify};

/// Policy applied at the coordinator admission boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildAdmissionMode {
    /// Deliberately bypass durable admission during rollout.
    Off,
    /// Record reservations but never deny at the configured reference cap.
    Observe,
    /// Atomically enforce the configured cap.
    #[default]
    Enforce,
}

/// Typed classification captured before dispatch; only the audited bypass weighs zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildWorkloadKind {
    TaskRun {
        role: TaskRunRole,
    },
    GraphWarmJob,
    /// Explicit, auditable non-build work. This is the only zero-slot class.
    NonBuild {
        audit_reason: &'static str,
    },
}

/// All currently dispatchable task-run roles are build-producing work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunRole {
    Worker,
    Reviewer,
    Lead,
    Planner,
    Architect,
    Advocate,
    Adversary,
    Judge,
}

impl TaskRunRole {
    /// Classify a known coordinator role. Unknown and missing values fail closed.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            Some("worker") => Some(Self::Worker),
            Some("reviewer") => Some(Self::Reviewer),
            Some("lead") => Some(Self::Lead),
            Some("planner") => Some(Self::Planner),
            Some("architect") => Some(Self::Architect),
            Some("advocate") => Some(Self::Advocate),
            Some("adversary") => Some(Self::Adversary),
            Some("judge") => Some(Self::Judge),
            _ => None,
        }
    }
}

/// Immutable identity fixed before capacity is reserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionRequest {
    pub domain: AdmissionDomain,
    pub work_id: String,
    pub generation: i64,
    pub object_name: String,
    pub kind: BuildWorkloadKind,
}

/// Admission decision returned to task dispatch callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildAdmissionDecision {
    Permitted {
        permit: WarmAdmissionPermit,
        idempotent: bool,
    },
    Denied {
        occupancy: i64,
        cap: i64,
    },
    /// Classification was absent or unrecognized. The observation counter is bounded.
    Unclassified,
}

#[derive(Clone, Debug)]
struct PermitState {
    key: AdmissionJournalKey,
    creator_server_epoch: String,
    object_name: String,
    durable: bool,
    released: bool,
}

/// A single controller shared by task-run dispatch and graph warming.
pub struct BuildAdmissionController {
    journal: Arc<AdmissionJournalRepository>,
    mode: BuildAdmissionMode,
    cap: i64,
    creator_server_epoch: String,
    permits: Mutex<HashMap<WarmAdmissionPermit, PermitState>>,
    permits_by_key: Mutex<HashMap<String, WarmAdmissionPermit>>,
    /// Runtime task-run IDs are learned when a session starts. This binding
    /// prevents a delayed terminal callback from selecting a later generation.
    permits_by_task_run: Mutex<HashMap<String, WarmAdmissionPermit>>,
    unclassified_observations: Mutex<u64>,
    would_defer_observations: Mutex<u64>,
    /// Enforce starts closed in production until recovery establishes durable
    /// occupancy. Observe deliberately remains available throughout telemetry
    /// degradation.
    ready: AtomicBool,
    released: Notify,
}

impl BuildAdmissionController {
    #[must_use]
    pub fn new(
        journal: Arc<AdmissionJournalRepository>,
        mode: BuildAdmissionMode,
        cap: i64,
        creator_server_epoch: impl Into<String>,
    ) -> Self {
        Self {
            journal,
            mode,
            cap,
            creator_server_epoch: creator_server_epoch.into(),
            permits: Mutex::new(HashMap::new()),
            permits_by_key: Mutex::new(HashMap::new()),
            permits_by_task_run: Mutex::new(HashMap::new()),
            unclassified_observations: Mutex::new(0),
            would_defer_observations: Mutex::new(0),
            ready: AtomicBool::new(true),
            released: Notify::new(),
        }
    }

    /// Construct an Enforce controller which cannot admit work until recovery opens it.
    #[must_use]
    pub fn new_closed(journal: Arc<AdmissionJournalRepository>, cap: i64, creator_server_epoch: impl Into<String>) -> Self {
        let controller = Self::new(journal, BuildAdmissionMode::Enforce, cap, creator_server_epoch);
        controller.ready.store(false, Ordering::Release);
        controller
    }

    /// Open the controller after recovery has established authoritative inventory.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Queue consumers may wait here after a terminal release instead of polling.
    #[must_use]
    pub fn release_notifier(&self) -> &Notify {
        &self.released
    }

    /// Durable inspection seam used by coordinator integration tests.
    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Arc<AdmissionJournalRepository> {
        &self.journal
    }

    /// Bounded count suitable for a telemetry exporter; values saturate at 1024.
    pub async fn unclassified_observation_count(&self) -> u64 {
        *self.unclassified_observations.lock().await
    }

    /// Bounded Observe-mode signal that the reference cap would have deferred work.
    pub async fn would_defer_observation_count(&self) -> u64 {
        *self.would_defer_observations.lock().await
    }

    pub async fn admit(
        &self,
        request: BuildAdmissionRequest,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        if self.mode == BuildAdmissionMode::Enforce && !self.is_ready() {
            return Ok(BuildAdmissionDecision::Denied { occupancy: 0, cap: self.cap });
        }
        let workload_kind = match request.kind {
            BuildWorkloadKind::TaskRun { .. } => match request.domain {
                AdmissionDomain::TaskObservation => AdmissionWorkloadKind::Task,
                AdmissionDomain::InvocationBuild => AdmissionWorkloadKind::Invocation,
                AdmissionDomain::WarmBuild => AdmissionWorkloadKind::Warm,
            },
            BuildWorkloadKind::GraphWarmJob => AdmissionWorkloadKind::Warm,
            BuildWorkloadKind::NonBuild { audit_reason } if !audit_reason.is_empty() => {
                return Ok(BuildAdmissionDecision::Permitted {
                    permit: WarmAdmissionPermit::new(),
                    idempotent: false,
                });
            }
            BuildWorkloadKind::NonBuild { .. } => {
                self.observe_unclassified().await;
                return Ok(BuildAdmissionDecision::Unclassified);
            }
        };
        let key = AdmissionJournalKey {
            domain: request.domain,
            work_id: request.work_id,
            generation: request.generation,
        };
        let permit_key = permit_key(&key);
        let durable = self.mode != BuildAdmissionMode::Off;
        let idempotent_permit = self.permits_by_key.lock().await.get(&permit_key).cloned();
        if let Some(permit) = idempotent_permit {
            return Ok(BuildAdmissionDecision::Permitted {
                permit,
                idempotent: true,
            });
        }
        let mut idempotent = false;
        if durable {
            let reservation = if self.mode == BuildAdmissionMode::Observe {
                let observed = self
                    .journal
                    .reserve_observed(
                        &ReserveAdmissionInput {
                            key: key.clone(),
                            workload_kind,
                            creator_server_epoch: self.creator_server_epoch.clone(),
                            object_name: request.object_name.clone(),
                        },
                        self.cap,
                    )
                    .await;
                let observed = match observed {
                    Ok(observed) => observed,
                    Err(error) => {
                        // Observe is telemetry-only: a journal outage must not become a dispatch denial.
                        tracing::warn!(%error, "build admission observation unavailable; permitting without journal telemetry");
                        return self.permit_without_reservation(key, permit_key, request.object_name).await;
                    }
                };
                if observed.would_defer {
                    let mut count = self.would_defer_observations.lock().await;
                    *count = count.saturating_add(1).min(1024);
                }
                observed.reservation
            } else {
                self.journal
                    .reserve(
                        &ReserveAdmissionInput {
                            key: key.clone(),
                            workload_kind,
                            creator_server_epoch: self.creator_server_epoch.clone(),
                            object_name: request.object_name.clone(),
                        },
                        self.cap,
                    )
                    .await
                    .map_err(unavailable)?
            };
            match reservation {
                ReserveAdmissionResult::Denied { occupancy, cap } => {
                    return Ok(BuildAdmissionDecision::Denied { occupancy, cap });
                }
                ReserveAdmissionResult::Reserved {
                    idempotent: value, ..
                } => idempotent = value,
            }
        }
        let permit = WarmAdmissionPermit::new();
        let state = PermitState {
            key: key.clone(),
            creator_server_epoch: self.creator_server_epoch.clone(),
            object_name: request.object_name,
            durable,
            released: false,
        };
        self.permits.lock().await.insert(permit.clone(), state);
        self.permits_by_key
            .lock()
            .await
            .insert(permit_key, permit.clone());
        Ok(BuildAdmissionDecision::Permitted { permit, idempotent })
    }

    async fn permit_without_reservation(
        &self,
        key: AdmissionJournalKey,
        permit_key: String,
        object_name: String,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let permit = WarmAdmissionPermit::new();
        self.permits.lock().await.insert(permit.clone(), PermitState {
            key,
            creator_server_epoch: self.creator_server_epoch.clone(),
            object_name,
            durable: false,
            released: false,
        });
        self.permits_by_key.lock().await.insert(permit_key, permit.clone());
        Ok(BuildAdmissionDecision::Permitted { permit, idempotent: false })
    }

    /// A missing or unknown task role is a fail-closed classification result.
    pub async fn admit_task_run(
        &self,
        role: Option<&str>,
        domain: AdmissionDomain,
        work_id: String,
        generation: i64,
        object_name: String,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let Some(role) = TaskRunRole::parse(role) else {
            self.observe_unclassified().await;
            return Ok(BuildAdmissionDecision::Unclassified);
        };
        self.admit(BuildAdmissionRequest {
            domain,
            work_id,
            generation,
            object_name,
            kind: BuildWorkloadKind::TaskRun { role },
        })
        .await
    }

    /// Return the retained permit for this exact task generation.
    pub async fn task_run_permit(
        &self,
        task_id: &str,
        generation: i64,
    ) -> Option<WarmAdmissionPermit> {
        self.permits
            .lock()
            .await
            .iter()
            .find(|(_, state)| {
                state.key.domain == AdmissionDomain::TaskObservation
                    && state.key.work_id == task_id
                    && state.key.generation == generation
            })
            .map(|(permit, _)| permit.clone())
    }

    /// Bind a UID-bearing runtime task-run to a permit already made Live.
    pub async fn bind_task_run(&self, task_run_id: String, permit: WarmAdmissionPermit) {
        self.permits_by_task_run
            .lock()
            .await
            .insert(task_run_id, permit);
    }

    /// Return only the permit bound to this runtime task-run UID. There is no
    /// task-ID fallback because that could release a newer reopened generation.
    pub async fn task_run_permit_for_runtime_id(
        &self,
        task_run_id: &str,
    ) -> Option<WarmAdmissionPermit> {
        self.permits_by_task_run
            .lock()
            .await
            .get(task_run_id)
            .cloned()
    }

    async fn observe_unclassified(&self) {
        let mut count = self.unclassified_observations.lock().await;
        *count = count.saturating_add(1).min(1024);
        tracing::warn!(
            observations = *count,
            "build admission classification missing or unknown; denying dispatch"
        );
    }

    async fn transition_permit(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        let Some(state) = self.permits.lock().await.get(permit).cloned() else {
            return Err(WarmAdmissionError::UnknownPermit);
        };
        if !state.durable {
            return Ok(());
        }
        let terminal = matches!(
            transition,
            WarmAdmissionTransition::DefinitiveFailure { .. }
                | WarmAdmissionTransition::Terminal { .. }
        );
        let result = match transition {
            WarmAdmissionTransition::CreateStarted => self
                .journal
                .mark_create_started(&CreateStartedInput {
                    key: state.key.clone(),
                    creator_server_epoch: state.creator_server_epoch,
                    object_name: state.object_name,
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::Live { uid } => self
                .journal
                .mark_live(&UidFencedAdmissionInput {
                    key: state.key.clone(),
                    object_uid: uid,
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::CreateUnknown { .. } => self
                .journal
                .mark_create_unknown(&state.key)
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::DefinitiveFailure { .. } => self
                .journal
                .mark_definitive_create_failure(&state.key)
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::Terminal { uid } => self
                .journal
                .mark_terminal(&TerminalAdmissionInput {
                    key: state.key.clone(),
                    object_uid: Some(uid),
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
        };
        if let Err(error) = result {
            if self.mode != BuildAdmissionMode::Observe {
                return Err(error);
            }
            tracing::warn!(%error, "build admission observation transition unavailable; continuing without journal telemetry");
        }
        if terminal {
            let newly_released = {
                let mut permits = self.permits.lock().await;
                match permits.get_mut(permit) {
                    Some(state) if !state.released => {
                        state.released = true;
                        true
                    }
                    Some(_) | None => false,
                }
            };
            if newly_released {
                // Retain one wakeup when the actor is currently handling the event
                // that performed this release and therefore has no `notified()`
                // future registered in its select loop.
                self.released.notify_one();
            }
        }
        Ok(())
    }
}

fn unavailable(error: impl std::fmt::Display) -> WarmAdmissionError {
    WarmAdmissionError::Unavailable {
        diagnostic: error.to_string(),
    }
}

fn permit_key(key: &AdmissionJournalKey) -> String {
    format!("{:?}:{}:{}", key.domain, key.work_id, key.generation)
}

#[async_trait]
impl WarmAdmission for BuildAdmissionController {
    async fn admit(
        &self,
        request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError> {
        let decision = self
            .admit(BuildAdmissionRequest {
                domain: AdmissionDomain::WarmBuild,
                work_id: request.work_id,
                generation: request.generation,
                object_name: request.object_name,
                kind: BuildWorkloadKind::GraphWarmJob,
            })
            .await?;
        match decision {
            BuildAdmissionDecision::Permitted { permit, .. } => Ok(permit),
            BuildAdmissionDecision::Denied { occupancy, cap } => Err(WarmAdmissionError::Denied {
                diagnostic: format!("occupancy {occupancy} reached cap {cap}"),
            }),
            BuildAdmissionDecision::Unclassified => Err(WarmAdmissionError::Denied {
                diagnostic: "unclassified build workload".into(),
            }),
        }
    }

    async fn transition(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        self.transition_permit(permit, transition).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::{
        AdmissionState, Database, ImageRepository, ProjectRepository,
        test_support::reject_admission_create_started_for_test,
    };
    use djinn_k8s::{
        K8sGraphWarmer, KubernetesConfig, WarmJobDispatcher, WarmJobManifest, WarmJobWatcher,
        WarmTerminalOutcome,
    };
    use djinn_runtime::GraphWarmerService;
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    fn controller(mode: BuildAdmissionMode, cap: i64) -> BuildAdmissionController {
        BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(
                Database::open_in_memory().unwrap(),
            )),
            mode,
            cap,
            "epoch",
        )
    }
    fn warm(id: &str) -> WarmAdmissionRequest {
        WarmAdmissionRequest {
            domain: "ignored".into(),
            work_id: id.into(),
            generation: 0,
            object_name: format!("job-{id}"),
        }
    }

    async fn seed_project_with_ready_image(db: &Database, name: &str) -> String {
        let projects = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = projects.create(name, "test", name).await.unwrap();
        let images = ImageRepository::new(db.clone());
        let image_id = format!("img-{name}");
        images.create(&image_id, name, None, "{}").await.unwrap();
        images
            .mark_ready(
                &image_id,
                &format!("reg.example:5000/djinn-project-{}:abc123", project.id),
                None,
            )
            .await
            .unwrap();
        images
            .set_project_image(&project.id, Some(&image_id))
            .await
            .unwrap();
        project.id
    }

    struct AdmissionStateRecordingDispatcher {
        journal: Arc<AdmissionJournalRepository>,
        work_id: String,
        posts: Arc<AtomicUsize>,
        posted: Arc<Notify>,
    }

    #[async_trait]
    impl WarmJobDispatcher for AdmissionStateRecordingDispatcher {
        async fn dispatch(
            &self,
            _namespace: &str,
            _job: WarmJobManifest,
        ) -> Result<String, String> {
            let history = self
                .journal
                .list_history(AdmissionDomain::WarmBuild, &self.work_id)
                .await
                .unwrap();
            assert_eq!(
                history[0].state,
                AdmissionState::CreateInFlight,
                "the concrete controller must durably record CreateStarted before POST"
            );
            self.posts.fetch_add(1, Ordering::SeqCst);
            self.posted.notify_one();
            Ok("warm-job".into())
        }
    }

    struct FencedTerminalWatcher;

    #[async_trait]
    impl WarmJobWatcher for FencedTerminalWatcher {
        async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
            WarmTerminalOutcome::Succeeded
        }

        async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
            Some("warm-uid".into())
        }
    }

    #[tokio::test]
    async fn concrete_k8s_warmer_shares_task_cap_and_retries_after_fenced_release() {
        let db = Database::open_in_memory().unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Enforce,
            1,
            "epoch",
        ));
        let project_id = seed_project_with_ready_image(&db, "shared-cap").await;
        let work_id = format!("graph-warm:{project_id}:unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let posted = Arc::new(Notify::new());
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db,
            Arc::new(AdmissionStateRecordingDispatcher {
                journal: Arc::clone(&journal),
                work_id: work_id.clone(),
                posts: Arc::clone(&posts),
                posted: Arc::clone(&posted),
            }),
            Arc::new(FencedTerminalWatcher),
        )
        .with_warm_admission(controller.clone());

        let task = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                1,
                "task-job".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: task, .. } = task else {
            panic!("the task must win the cap-one reservation");
        };

        warmer.trigger(&project_id).await;
        assert_eq!(posts.load(Ordering::SeqCst), 0, "denied warm must not POST");
        assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 1);
        assert!(
            journal
                .list_history(AdmissionDomain::WarmBuild, &work_id)
                .await
                .unwrap()
                .is_empty(),
            "the denied warm does not become completed or failed"
        );

        controller
            .transition(&task, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &task,
                WarmAdmissionTransition::Live {
                    uid: "task-uid".into(),
                },
            )
            .await
            .unwrap();
        let released = controller.release_notifier().notified();
        controller
            .transition(
                &task,
                WarmAdmissionTransition::Terminal {
                    uid: "task-uid".into(),
                },
            )
            .await
            .unwrap();
        released.await;

        let post = posted.notified();
        tokio::pin!(post);
        tokio::time::timeout(std::time::Duration::from_secs(3), post)
            .await
            .expect("pending warm should retry after the admission backoff");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "released capacity retries the pending warm"
        );
    }

    #[tokio::test]
    async fn concrete_k8s_warmer_keeps_failed_create_started_pending_without_posting() {
        let db = Database::open_in_memory().unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Enforce,
            1,
            "epoch",
        ));
        let project_id = seed_project_with_ready_image(&db, "create-started-failure").await;
        let work_id = format!("graph-warm:{project_id}:unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let posted = Arc::new(Notify::new());
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db.clone(),
            Arc::new(AdmissionStateRecordingDispatcher {
                journal: Arc::clone(&journal),
                work_id: work_id.clone(),
                posts: Arc::clone(&posts),
                posted: Arc::clone(&posted),
            }),
            Arc::new(FencedTerminalWatcher),
        )
        .with_warm_admission(controller);

        // Fail the real controller durable state transition after it reserves
        // the warm row, rather than substituting a fake WarmAdmission.
        reject_admission_create_started_for_test(&db, true).await;

        warmer.trigger(&project_id).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "a real-controller CreateStarted failure must perform zero POSTs"
        );
        assert_eq!(
            journal
                .list_history(AdmissionDomain::WarmBuild, &work_id)
                .await
                .unwrap()[0]
                .state,
            AdmissionState::Reserved,
            "the failed transition retains the coalesced warm reservation"
        );
        warmer.trigger(&project_id).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "an immediate retrigger coalesces onto the pending warm"
        );

        reject_admission_create_started_for_test(&db, false).await;
        let post = posted.notified();
        tokio::pin!(post);
        tokio::time::timeout(std::time::Duration::from_secs(3), post)
            .await
            .expect("pending warm should retry after the admission backoff");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "the retained warm retries after the journal transition becomes durable"
        );
    }

    #[test]
    fn classification_covers_every_dispatch_role_and_rejects_unknown() {
        for role in [
            "worker",
            "reviewer",
            "lead",
            "planner",
            "architect",
            "advocate",
            "adversary",
            "judge",
        ] {
            assert!(TaskRunRole::parse(Some(role)).is_some());
        }
        assert_eq!(TaskRunRole::parse(None), None);
        assert_eq!(TaskRunRole::parse(Some("mystery")), None);
    }

    #[tokio::test]
    async fn off_is_noop_and_unknown_is_bounded() {
        let controller = controller(BuildAdmissionMode::Off, 0);
        let permit = WarmAdmission::admit(&controller, warm("off"))
            .await
            .unwrap();
        controller
            .transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            0
        );
        for _ in 0..1025 {
            let _ = controller
                .admit_task_run(
                    None,
                    AdmissionDomain::TaskObservation,
                    "x".into(),
                    0,
                    "x".into(),
                )
                .await;
        }
        assert_eq!(controller.unclassified_observation_count().await, 1024);
    }

    #[tokio::test]
    async fn observe_permits_when_journal_reservation_is_unavailable() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        db.pool().close().await;
        let controller = BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(db)),
            BuildAdmissionMode::Observe,
            1,
            "epoch",
        );

        assert!(
            WarmAdmission::admit(&controller, warm("journal-down"))
                .await
                .is_ok(),
            "Observe journal failures are telemetry-only and must not defer dispatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_records_serialized_would_defer_without_denial_and_enforce_combines_domains() {
        let observed = Arc::new(controller(BuildAdmissionMode::Observe, 1));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first = {
            let observed = Arc::clone(&observed);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                WarmAdmission::admit(observed.as_ref(), warm("a")).await
            })
        };
        let second = {
            let observed = Arc::clone(&observed);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                WarmAdmission::admit(observed.as_ref(), warm("b")).await
            })
        };
        assert!(first.await.unwrap().is_ok());
        assert!(second.await.unwrap().is_ok());
        assert_eq!(observed.would_defer_observation_count().await, 1);
        let enforced = controller(BuildAdmissionMode::Enforce, 1);
        let _ = enforced
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                0,
                "task-job".into(),
            )
            .await
            .unwrap();
        assert!(matches!(
            WarmAdmission::admit(&enforced, warm("warm")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
    }

    #[tokio::test]
    async fn permits_are_idempotent_and_terminal_notifies_and_is_uid_fenced() {
        let controller = controller(BuildAdmissionMode::Enforce, 2);
        let first = WarmAdmission::admit(&controller, warm("same"))
            .await
            .unwrap();
        let second = WarmAdmission::admit(&controller, warm("same"))
            .await
            .unwrap();
        assert_eq!(first, second);
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(&first, WarmAdmissionTransition::Live { uid: "uid".into() })
            .await
            .unwrap();
        assert!(
            controller
                .transition(
                    &first,
                    WarmAdmissionTransition::Terminal {
                        uid: "wrong".into()
                    }
                )
                .await
                .is_err()
        );
        let notified = controller.release_notifier().notified();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal { uid: "uid".into() },
            )
            .await
            .unwrap();
        notified.await;
        assert_eq!(
            controller
                .journal
                .list_history(AdmissionDomain::WarmBuild, "same")
                .await
                .unwrap()[0]
                .state,
            AdmissionState::Terminal
        );
    }

    #[tokio::test]
    async fn task_generations_and_runtime_uids_fence_terminal_release() {
        let controller = controller(BuildAdmissionMode::Enforce, 3);
        let first = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                1,
                "task-run-task-1".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: first, .. } = first else {
            panic!("task generation one must be admitted");
        };
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Live {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_some(),
            "generation one release must retain exactly one wakeup"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "generation one release must not retain a second wakeup"
        );

        // Repeating the matching terminal callback while generation one is
        // still current is idempotent and does not emit another wakeup.
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "duplicate generation-one terminal must not wake dispatch again"
        );

        let second = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                2,
                "task-run-task-2".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: second, .. } = second else {
            panic!("task generation two must be admitted");
        };
        controller
            .transition(&second, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &second,
                WarmAdmissionTransition::Live {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();

        // Once generation two exists, a delayed callback for the old
        // generation is stale and cannot release the newer row.
        let error = controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .expect_err("generation-one callback must be rejected as stale");
        assert_eq!(
            error,
            WarmAdmissionError::Unavailable {
                diagnostic: "invalid transition: stale admission generation 1 for task".into(),
            }
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "delayed old-generation callback must not wake dispatch"
        );
        let history = controller
            .journal
            .list_history(AdmissionDomain::TaskObservation, "task")
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .find(|row| row.key.generation == 2)
                .unwrap()
                .state,
            AdmissionState::Live
        );
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1,
            "delayed old-generation duplicate must leave generation two occupied"
        );

        // A wrong UID and an unbound (UID-less) callback retain occupancy.
        assert!(
            controller
                .transition(
                    &second,
                    WarmAdmissionTransition::Terminal {
                        uid: "uid-one".into(),
                    },
                )
                .await
                .is_err()
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "wrong generation-two UID must not wake dispatch"
        );
        assert!(
            controller
                .task_run_permit_for_runtime_id("missing-uid")
                .await
                .is_none()
        );
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1,
            "UID-less terminal handling must retain generation-two occupancy"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "UID-less terminal handling must not wake dispatch"
        );

        controller
            .transition(
                &second,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_some(),
            "matching generation-two terminal must retain one wakeup"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "matching generation-two terminal must retain only one wakeup"
        );

        // A duplicate matching terminal callback is idempotent.
        controller
            .transition(
                &second,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "duplicate generation-two terminal must not wake dispatch again"
        );
    }
    #[tokio::test]
    async fn closed_enforce_controller_denies_until_recovery_marks_ready() {
        let controller = BuildAdmissionController::new_closed(
            Arc::new(AdmissionJournalRepository::new(Database::open_in_memory().unwrap())),
            1,
            "epoch",
        );
        assert!(!controller.is_ready());
        assert!(matches!(
            WarmAdmission::admit(&controller, warm("closed")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
        controller.mark_ready();
        assert!(WarmAdmission::admit(&controller, warm("open")).await.is_ok());
    }

}
