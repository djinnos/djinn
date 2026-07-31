//! Post-admission ceiling capture and birth downsize, before dispatch.
//!
//! This is where proposal `3i92` stops being building blocks and starts
//! governing real Pods. It composes four merged slices and adds no new
//! Kubernetes or SQL primitives of its own:
//!
//! * `4acu` — [`BuildPodPermitRepository::capture_resize_identity`], the
//!   write-once identity, and the **partial unique index on `pod_uid`**.
//! * `g8jk-1` — the `resize-v2`-only launcher CPU ceiling in the render.
//! * `g8jk-2` — [`djinn_k8s::pod_resize`], the limits-only `pods/resize`
//!   client that confirms through `status.initContainerStatuses`.
//! * `g8jk-4` — the namespaced, patch-only `pods/resize` RBAC rule.
//!
//! # The sequence
//!
//! Once the Pod has a UID and the launcher sidecar is admitted: **fresh GET**,
//! read the **persisted**
//! `spec.initContainers[name=cgroup-launcher].resources.limits.cpu`. That value
//! is the admitted ceiling — it includes any mutating-admission result
//! *precisely because* it comes from the stored Pod rather than the render
//! input. Write it once, with the Pod UID and the effective protocol. **Then**
//! resize down to [`BIRTH_CPU_MILLICORES`] and confirm through
//! `status.initContainerStatuses`. Only after that confirmation may the worker
//! session be admitted to dispatch.
//!
//! A test that compares the stored ceiling against the *render input* proves
//! nothing and would pass on a broken implementation: the whole reason capture
//! happens after admission is that a mutating webhook may have changed what was
//! rendered.
//!
//! # `birth_confirmed` is a capture state, not a confirmation state
//!
//! Migration 164's `capture_resize_identity` sets `state = 'birth_confirmed'`
//! as part of the *capture*, which happens before the birth resize is even
//! issued. The durable state name therefore overstates: a row sitting in
//! `birth_confirmed` proves an identity was captured, **not** that the launcher
//! is at 250m.
//!
//! Two consequences, both load-bearing:
//!
//! 1. The dispatch gate keys on the in-process confirmed outcome of *this*
//!    bootstrap (see [`DispatchGate`]), never on the row's state.
//! 2. A bootstrap resumed after a server restart re-issues the birth resize
//!    for a row already in `birth_confirmed`. That is idempotent by
//!    construction — resize to a value the launcher already holds confirms
//!    immediately.
//!
//! # Why the ceiling is read from the row, not re-derived
//!
//! Re-observation after a successful birth downsize reads **250m**, because
//! that is what the Pod now declares. The captured ceiling therefore has to
//! come from the durable row on every resumed pass, and capture may only be
//! *attempted* when the row carries no identity yet. Deriving the ceiling from
//! a fresh observation on resume would compare 250m against the stored ceiling,
//! call it a conflicting recapture, and delete a healthy Pod.
//!
//! # `leaf-v1` never reaches capture, and cannot
//!
//! `g8jk-1` renders `limits.cpu` on the launcher under `resize-v2` **only** —
//! under `leaf-v1` a container limit there is an ancestor clamp over every
//! invocation leaf (task `7deu` measured a leaf set to 4 cores burning 0.25).
//! Migration 164 in turn requires `admitted_cpu_millicores > 0` for any
//! identity at all. So a `leaf-v1` Pod has no ceiling to capture and the schema
//! would reject one if it did: bootstrap is structurally `resize-v2`-only, and
//! `leaf-v1` dispatch is untouched by this module.
//!
//! # Replacement Pods: reject-and-delete
//!
//! See [`RefusalReason::ConflictingCapture`]. `build_pod_permits` has PRIMARY
//! KEY `task_run_id`, `capture_resize_identity` predicates on
//! `state = 'job_created' AND pod_uid IS NULL`, and the migration-164 trigger
//! makes a captured identity immutable. Exactly one capture per task run is
//! possible, ever.
//!
//! Release-and-reacquire was considered and is **not available**:
//! [`BuildPodPermitRepository::acquire`] deliberately refuses to resurrect a
//! released row under the same task-run id (`AlreadyReleased`), and the trigger
//! has no edge back to `job_created`. Making it available would take a new
//! migration and a change to `build_pod_permit.rs` — that is, it would take
//! dismantling the write-once guarantee this design rests on.
//!
//! So a replacement Pod for the same task run is refused and UID-fenced
//! deleted, and the run is not dispatched. Recovery is a **new task run**, with
//! a new `task_run_id` and therefore a new permit. This is stated here rather
//! than left for someone to discover during an incident.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use djinn_agent::task_run_resize_admission::{
    ResizeAdmissionOutcome, ResizeAdmissionRefused, ResizeAdmissionRequest, TaskRunResizeAdmission,
};
use djinn_db::{
    BuildPodPermitRepository, BuildPodPermitRow, BuildPodPermitState, BuildPodResizeIdentity,
    CaptureBuildPodResizeIdentityResult, Error as DbError,
};
use djinn_k8s::launcher::LAUNCHER_CPU_REQUEST_MILLICORES;
use djinn_k8s::pod_resize::PodResizeError;
use djinn_k8s::runtime::{
    LauncherObservationError, ObservedLauncherSidecar, TaskRunPodResizeSurface,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use tracing::{info, warn};

/// The CPU limit every `resize-v2` launcher sidecar is downsized to before its
/// worker session may dispatch. Lifts move it up, bounded by the captured
/// ceiling; that lifecycle belongs to `0ppk`.
pub const BIRTH_CPU_MILLICORES: u64 = 250;

/// The Postgres SQLSTATE for a unique-violation. It is the *only* signal that
/// distinguishes "another permit already owns this Pod UID" from "the database
/// did not answer", and those two must not be conflated: the first is a
/// permanent authority conflict that must delete the Pod, the second is a
/// transient condition that must not.
const UNIQUE_VIOLATION: &str = "23505";

// ── The apiserver seam ─────────────────────────────────────────────────────

/// The three Kubernetes operations bootstrap performs.
///
/// Behind a trait so the whole sequence is drivable from fixtures with no
/// cluster, and so this crate needs no `kube` / `k8s-openapi` dependency of its
/// own. The production implementation is
/// [`djinn_k8s::runtime::TaskRunPodResizeSurface`].
#[async_trait]
pub trait TaskRunPodSurface: Send + Sync {
    /// Fresh GET of the single Pod labelled for this task run, flattened to the
    /// launcher facts. `Ok(None)` means no Pod exists yet.
    ///
    /// # Errors
    ///
    /// [`LauncherObservationError`] when the read fails, the Pod is not fully
    /// admitted, or the launcher cannot be uniquely named.
    async fn observe_launcher(
        &self,
        task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError>;

    /// One limits-only resize of the launcher sidecar, confirmed through
    /// `status.initContainerStatuses`.
    ///
    /// # Errors
    ///
    /// [`PodResizeError`]; `NotConfirmed` when the PATCH was accepted but the
    /// fresh status does not yet agree.
    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError>;

    /// UID-fenced destruction of exactly the observed Pod.
    ///
    /// # Errors
    ///
    /// The rendered failure from the fenced-deletion protocol.
    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String>;
}

#[async_trait]
impl TaskRunPodSurface for TaskRunPodResizeSurface {
    async fn observe_launcher(
        &self,
        task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        Self::observe_launcher(self, task_run_id).await
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
        Self::resize_launcher_cpu(self, pod_name, target_millicores).await
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        Self::uid_fenced_delete(self, task_run_id, pod_uid).await
    }
}

// ── Inputs and outcomes ────────────────────────────────────────────────────

/// The permit a bootstrap acts under. Every durable write is fenced by all
/// three fields, so a stale actor cannot advance a lifecycle it no longer owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermitBinding {
    /// Task run whose permit row this is (`build_pod_permits` PRIMARY KEY).
    pub task_run_id: String,
    /// Immutable permit identity.
    pub permit_id: String,
    /// Monotonic ownership fence.
    pub fencing_token: i64,
}

/// Why bootstrap could not proceed *yet*. Every variant is retryable and none
/// of them touches the Pod.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotReady {
    /// No Pod carries this task run's label yet.
    #[error("no Pod exists for this task run yet")]
    PodAbsent,
    /// The permit row has no Job UID bound yet, so `capture_resize_identity`'s
    /// `state = 'job_created'` predicate cannot hold.
    #[error("permit is still `acquired`; no Job UID has been bound")]
    JobNotBound,
    /// A `metadata` field the fence depends on is not populated yet.
    #[error("stored Pod is not fully admitted: {0}")]
    PodIncomplete(String),
    /// The kubelet has not started the sidecar, so it has no container ID or
    /// resolved image ID to fence the identity with.
    #[error("launcher sidecar has not started: `{field}` is absent")]
    LauncherNotStarted {
        /// Which status field is still absent.
        field: &'static str,
    },
    /// The PATCH was accepted but the fresh init-container status does not
    /// agree yet. Backoff and the 30s confirmation deadline belong to `0ppk`.
    #[error("birth downsize not confirmed yet: {0}")]
    BirthUnconfirmed(String),
    /// The apiserver call failed. Transport failures are not authority
    /// failures.
    #[error("kubernetes unavailable: {0}")]
    KubernetesUnavailable(String),
    /// Durable storage could not answer. Deliberately **not** a refusal: a Pod
    /// must not be destroyed because Postgres blinked.
    #[error("durable storage unavailable: {0}")]
    StorageUnavailable(String),
}

/// Why bootstrap refused, permanently, for this Pod.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefusalReason {
    /// The launcher is not uniquely identifiable in `spec.initContainers` or in
    /// `status.initContainerStatuses`. Zero and two are the same error: both
    /// mean the resize target cannot be named, and resolving either by
    /// positional index would address the wrong container.
    #[error("launcher identity is unusable: {0}")]
    LauncherNotNameable(String),
    /// `resize-v2` was resolved but the stored Pod declares no launcher CPU
    /// limit. Under this protocol the launcher writes no leaf quota, so a
    /// missing container limit means a brokered build is bounded only by the
    /// node.
    #[error("resize-v2 Pod declares no launcher CPU ceiling")]
    NoAdmittedCeiling,
    /// The admitted ceiling is below the launcher's own CPU request. Kubernetes
    /// would reject such a container outright; if one is nevertheless stored,
    /// the Pod is not one we can govern.
    #[error("admitted ceiling {ceiling}m is below the launcher CPU request {request}m")]
    CeilingBelowLauncherRequest {
        /// Millicores read back from the stored Pod.
        ceiling: u64,
        /// The launcher sidecar's own request, in millicores.
        request: u64,
    },
    /// The admitted ceiling is below the birth limit, so the "birth downsize"
    /// would be an *upsize* past the admitted ceiling. Refused rather than
    /// oversubscribed.
    #[error("admitted ceiling {ceiling}m is below the birth limit {birth}m")]
    CeilingBelowBirthLimit {
        /// Millicores read back from the stored Pod.
        ceiling: u64,
        /// [`BIRTH_CPU_MILLICORES`].
        birth: u64,
    },
    /// The stored Pod reports a different authority protocol than the server
    /// resolved for it. Exactly one authority governs an admitted Pod.
    #[error("launcher reports protocol `{observed}` but the server resolved `{effective}`")]
    ProtocolMismatch {
        /// What the stored launcher sidecar declares.
        observed: String,
        /// What the server resolved for this dispatch.
        effective: String,
    },
    /// The stored Pod declares no protocol at all under `resize-v2`. Mapping a
    /// missing handshake onto an effective protocol is the signed legacy-digest
    /// allowlist's job (`vf7a`) and is never permitted under resize authority.
    #[error("launcher declares no authority protocol under resize-v2")]
    ProtocolMissing,
    /// A `leaf-v1` Pod carries a launcher CPU limit. Under `leaf-v1` the
    /// launcher owns each invocation leaf's `cpu.max`, so a container limit
    /// here is an ancestor clamp that silently throttles every build — task
    /// `7deu`'s measured defect. The render never produces this, so its
    /// presence means something else wrote it.
    #[error("leaf-v1 Pod carries a launcher CPU ceiling of {ceiling}m")]
    LeafAuthorityCarriesCeiling {
        /// Millicores read back from the stored Pod.
        ceiling: u64,
    },
    /// The permit row already holds an identity, and it is not this Pod's — or
    /// the repository refused the capture outright. **This is the
    /// replacement-Pod path.** See the module docs: reject-and-delete, because
    /// exactly one capture per task run is possible and release-and-reacquire
    /// does not exist.
    #[error("permit already holds a conflicting resize identity")]
    ConflictingCapture,
    /// Another permit row already owns this Pod UID. Caught by
    /// `build_pod_permits_pod_uid_key`, migration 164's partial unique index —
    /// the per-row write-once predicate cannot see across task runs, so without
    /// that index two runs would each capture the same Pod with different
    /// ceilings and both believe they held authority.
    #[error("Pod UID is already captured by another permit")]
    PodUidClaimedByAnotherPermit,
    /// The permit row is not the one this caller holds. A stale owner must not
    /// destroy the live owner's Pod, so this refusal retains it.
    #[error("permit identity or fencing token does not match the durable row")]
    StalePermit,
    /// The row has moved past bootstrap into the lift/drop lifecycle that
    /// `0ppk` owns. Not ours to re-drive, and not ours to delete.
    #[error("permit is in resize state `{state}`, past bootstrap")]
    PastBootstrap {
        /// The durable state observed.
        state: String,
    },
    /// The launcher cannot be resized at all: an ambiguous target, or a CPU
    /// quantity the client refuses to build a PATCH from.
    #[error("birth downsize is unissuable: {0}")]
    BirthUnissuable(String),
}

/// What happened to the Pod as a result of a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PodDisposition {
    /// The Pod was left alone — either nothing exists to fence, or destroying
    /// it is not this caller's right.
    Retained,
    /// A UID-fenced delete was issued for exactly the observed Pod. The inner
    /// result is the delete's own outcome: a failed delete does not turn a
    /// refusal into an admission.
    UidFencedDeleted(Result<(), String>),
}

/// The result of one bootstrap pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// `resize-v2`: the admitted ceiling is durable, the birth limit is
    /// confirmed through `status.initContainerStatuses`, and dispatch is
    /// admitted.
    BirthConfirmed {
        /// The Pod this run is fenced to.
        pod_uid: String,
        /// The write-once ceiling, read back from the durable row.
        admitted_cpu_millicores: i64,
    },
    /// `leaf-v1`: nothing to capture and nothing to resize. Dispatch proceeds
    /// under launcher leaf authority, exactly as it did before this module.
    LeafAuthority,
    /// Retry. The Pod is untouched and dispatch is not admitted.
    NotReady(NotReady),
    /// Fail closed. Dispatch is not admitted, permanently for this task run.
    Refused {
        /// Why.
        reason: RefusalReason,
        /// What happened to the Pod.
        disposition: PodDisposition,
    },
}

impl BootstrapOutcome {
    /// Whether this outcome admits the worker session to dispatch.
    #[must_use]
    pub const fn admits_dispatch(&self) -> bool {
        matches!(self, Self::BirthConfirmed { .. } | Self::LeafAuthority)
    }
}

// ── The dispatch gate ──────────────────────────────────────────────────────

/// Proof that a task run's birth limit is confirmed (or that it is a `leaf-v1`
/// run that never had one).
///
/// The constructor is private to this module, so the only way to obtain one is
/// [`DispatchGate::admit`] after a bootstrap that actually confirmed. A caller
/// that needs this token to dispatch cannot dispatch early by mistake — it
/// cannot build the token at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchAdmission {
    task_run_id: String,
    authority: AdmittedAuthority,
}

/// Which authority governs an admitted run's CPU quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmittedAuthority {
    /// The launcher owns each invocation leaf's `cpu.max`.
    Leaf,
    /// Kubernetes owns the launcher container's limit; the birth downsize is
    /// confirmed.
    Resize,
}

impl DispatchAdmission {
    /// The task run this admission is for.
    #[must_use]
    pub fn task_run_id(&self) -> &str {
        &self.task_run_id
    }

    /// Which authority governs it.
    #[must_use]
    pub const fn authority(&self) -> AdmittedAuthority {
        self.authority
    }
}

/// Why dispatch was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DispatchRefused {
    /// No bootstrap has confirmed a birth limit for this task run. An unknown
    /// task run lands here too: fail closed, a run this module has never seen
    /// is not a run whose quota authority is established.
    #[error("birth limit is not confirmed for task run {task_run_id}")]
    BirthNotConfirmed {
        /// The refused task run.
        task_run_id: String,
    },
}

/// The gate that stands between a confirmed birth downsize and dispatch.
#[derive(Debug, Default)]
pub struct DispatchGate {
    admitted: Mutex<HashMap<String, AdmittedAuthority>>,
    dispatches_before_birth_confirmation: AtomicU64,
}

impl DispatchGate {
    /// A gate that admits nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn record_admissible(&self, task_run_id: &str, authority: AdmittedAuthority) {
        self.admitted
            .lock()
            .expect("dispatch gate poisoned")
            .insert(task_run_id.to_owned(), authority);
    }

    fn authority_of(&self, task_run_id: &str) -> Option<AdmittedAuthority> {
        self.admitted
            .lock()
            .expect("dispatch gate poisoned")
            .get(task_run_id)
            .copied()
    }

    /// Admit a task run to dispatch, or refuse it.
    ///
    /// # Errors
    ///
    /// [`DispatchRefused::BirthNotConfirmed`] when no bootstrap has confirmed
    /// this task run — including when it has never been seen at all.
    pub fn admit(&self, task_run_id: &str) -> Result<DispatchAdmission, DispatchRefused> {
        match self.authority_of(task_run_id) {
            Some(authority) => Ok(DispatchAdmission {
                task_run_id: task_run_id.to_owned(),
                authority,
            }),
            None => Err(DispatchRefused::BirthNotConfirmed {
                task_run_id: task_run_id.to_owned(),
            }),
        }
    }

    /// Called by the dispatch site at the moment a worker session actually
    /// starts.
    ///
    /// This exists to make the gate's *absence* observable rather than merely
    /// assertable. With [`Self::admit`] standing in front of the dispatch site
    /// this counter is structurally unreachable and stays zero; delete the gate
    /// and it becomes non-zero on the first early dispatch, whether or not
    /// anyone remembered to write an assertion about ordering.
    pub fn record_dispatch_started(&self, task_run_id: &str) {
        if self.authority_of(task_run_id).is_none() {
            self.dispatches_before_birth_confirmation
                .fetch_add(1, Ordering::SeqCst);
            warn!(
                task_run_id,
                "task_run_resize_bootstrap: dispatch started before the birth limit was confirmed"
            );
        }
    }

    /// How many dispatches have started without a confirmed birth limit.
    #[must_use]
    pub fn dispatches_before_birth_confirmation(&self) -> u64 {
        self.dispatches_before_birth_confirmation
            .load(Ordering::SeqCst)
    }
}

// ── The bootstrap ──────────────────────────────────────────────────────────

/// Composes the durable permit relation, the `pods/resize` client, and the
/// UID-fenced deletion protocol into one pass.
pub struct TaskRunResizeBootstrap {
    permits: BuildPodPermitRepository,
    surface: Arc<dyn TaskRunPodSurface>,
    gate: Arc<DispatchGate>,
}

impl TaskRunResizeBootstrap {
    /// Wire a bootstrap to a permit repository, an apiserver surface, and a
    /// gate.
    #[must_use]
    pub fn new(
        permits: BuildPodPermitRepository,
        surface: Arc<dyn TaskRunPodSurface>,
        gate: Arc<DispatchGate>,
    ) -> Self {
        Self {
            permits,
            surface,
            gate,
        }
    }

    /// The gate this bootstrap admits through.
    #[must_use]
    pub fn gate(&self) -> &Arc<DispatchGate> {
        &self.gate
    }

    /// Run one bootstrap pass for `permit` under the server-resolved
    /// `effective` protocol.
    ///
    /// Idempotent: a pass that has already captured and confirmed returns
    /// [`BootstrapOutcome::BirthConfirmed`] again without a second capture, and
    /// re-issues a resize the launcher already satisfies.
    pub async fn bootstrap(
        &self,
        permit: &PermitBinding,
        effective: LauncherAuthorityProtocol,
    ) -> BootstrapOutcome {
        // 1. The durable row first. It decides whether capture is even
        //    attemptable, and on a resumed pass it — not a fresh observation —
        //    is the authority on the captured ceiling.
        let row = match self.permits.active(&permit.task_run_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                return BootstrapOutcome::Refused {
                    reason: RefusalReason::StalePermit,
                    disposition: PodDisposition::Retained,
                };
            }
            Err(error) => {
                return BootstrapOutcome::NotReady(NotReady::StorageUnavailable(error.to_string()));
            }
        };
        if row.permit_id != permit.permit_id || row.fencing_token != permit.fencing_token {
            return BootstrapOutcome::Refused {
                reason: RefusalReason::StalePermit,
                disposition: PodDisposition::Retained,
            };
        }
        match row.state {
            BuildPodPermitState::Acquired => {
                return BootstrapOutcome::NotReady(NotReady::JobNotBound);
            }
            BuildPodPermitState::JobCreated | BuildPodPermitState::BirthConfirmed => {}
            other => {
                return BootstrapOutcome::Refused {
                    reason: RefusalReason::PastBootstrap {
                        state: format!("{other:?}"),
                    },
                    disposition: PodDisposition::Retained,
                };
            }
        }

        // 2. Fresh GET of the stored Pod. Never the render input: capture
        //    exists to observe what admission actually produced.
        let observed = match self.surface.observe_launcher(&permit.task_run_id).await {
            Ok(Some(observed)) => observed,
            Ok(None) => return BootstrapOutcome::NotReady(NotReady::PodAbsent),
            Err(LauncherObservationError::Incomplete { field }) => {
                return BootstrapOutcome::NotReady(NotReady::PodIncomplete(format!(
                    "metadata.{field}"
                )));
            }
            Err(LauncherObservationError::Api(error)) => {
                return BootstrapOutcome::NotReady(NotReady::KubernetesUnavailable(error));
            }
            Err(error @ LauncherObservationError::Ambiguous(_)) => {
                // No Pod UID is available here by construction — the launcher
                // could not be named, so there is nothing to fence a delete to.
                return BootstrapOutcome::Refused {
                    reason: RefusalReason::LauncherNotNameable(error.to_string()),
                    disposition: PodDisposition::Retained,
                };
            }
        };

        // 3. Protocol agreement, before anything durable is written.
        if let Some(outcome) = self.check_protocol(permit, &observed, effective).await {
            return outcome;
        }
        if effective.launcher_owns_leaf_quota() {
            // leaf-v1: no ceiling exists to capture and migration 164 would
            // reject an identity without one. Dispatch under leaf authority,
            // exactly as it did before this module existed.
            self.gate
                .record_admissible(&permit.task_run_id, AdmittedAuthority::Leaf);
            return BootstrapOutcome::LeafAuthority;
        }

        // 4. Establish the write-once ceiling: from the durable row when one is
        //    already captured, otherwise by capturing this observation.
        let ceiling = match row.resize_identity.as_ref() {
            Some(identity) => {
                if identity.pod_uid != observed.pod_uid {
                    // The replacement-Pod path. See the module docs.
                    return self
                        .refuse_and_delete(permit, &observed, RefusalReason::ConflictingCapture)
                        .await;
                }
                identity.admitted_cpu_millicores
            }
            None => match self.capture(permit, &observed, effective).await {
                Ok(row) => match row.resize_identity {
                    Some(identity) => identity.admitted_cpu_millicores,
                    None => {
                        // A captured row without an identity is impossible under
                        // `build_pod_permits_resize_state_identity_check`; treat
                        // it as storage the caller cannot trust rather than
                        // inventing a ceiling.
                        return BootstrapOutcome::NotReady(NotReady::StorageUnavailable(
                            "captured permit row carries no resize identity".to_owned(),
                        ));
                    }
                },
                Err(outcome) => return outcome,
            },
        };

        // 5. Birth downsize, confirmed through `status.initContainerStatuses`.
        //    Only now is the ceiling a number this module has authority over.
        match self
            .surface
            .resize_launcher_cpu(&observed.pod_name, BIRTH_CPU_MILLICORES)
            .await
        {
            Ok(()) => {}
            Err(PodResizeError::NotConfirmed(reason)) => {
                return BootstrapOutcome::NotReady(NotReady::BirthUnconfirmed(reason.to_string()));
            }
            Err(error @ PodResizeError::Api { .. }) => {
                return BootstrapOutcome::NotReady(NotReady::KubernetesUnavailable(
                    error.to_string(),
                ));
            }
            Err(error) => {
                return self
                    .refuse_and_delete(
                        permit,
                        &observed,
                        RefusalReason::BirthUnissuable(error.to_string()),
                    )
                    .await;
            }
        }

        self.gate
            .record_admissible(&permit.task_run_id, AdmittedAuthority::Resize);
        info!(
            task_run_id = %permit.task_run_id,
            pod_uid = %observed.pod_uid,
            admitted_cpu_millicores = ceiling,
            birth_millicores = BIRTH_CPU_MILLICORES,
            "task_run_resize_bootstrap: birth limit confirmed; dispatch admitted"
        );
        BootstrapOutcome::BirthConfirmed {
            pod_uid: observed.pod_uid,
            admitted_cpu_millicores: ceiling,
        }
    }

    /// Protocol agreement and the ceiling's own sanity, both of which must hold
    /// before any durable write. Returns `Some(outcome)` when bootstrap must
    /// stop.
    async fn check_protocol(
        &self,
        permit: &PermitBinding,
        observed: &ObservedLauncherSidecar,
        effective: LauncherAuthorityProtocol,
    ) -> Option<BootstrapOutcome> {
        if let Some(declared) = observed.observed_protocol.as_deref()
            && declared != effective.as_wire()
        {
            return Some(
                self.refuse_and_delete(
                    permit,
                    observed,
                    RefusalReason::ProtocolMismatch {
                        observed: declared.to_owned(),
                        effective: effective.as_wire().to_owned(),
                    },
                )
                .await,
            );
        }

        if effective.launcher_owns_leaf_quota() {
            // A leaf-v1 Pod must carry no launcher CPU limit at all.
            return match observed.admitted_cpu_millicores {
                Some(ceiling) => Some(
                    self.refuse_and_delete(
                        permit,
                        observed,
                        RefusalReason::LeafAuthorityCarriesCeiling { ceiling },
                    )
                    .await,
                ),
                None => None,
            };
        }

        if observed.observed_protocol.is_none() {
            return Some(
                self.refuse_and_delete(permit, observed, RefusalReason::ProtocolMissing)
                    .await,
            );
        }
        let Some(ceiling) = observed.admitted_cpu_millicores else {
            return Some(
                self.refuse_and_delete(permit, observed, RefusalReason::NoAdmittedCeiling)
                    .await,
            );
        };
        if ceiling < u64::from(LAUNCHER_CPU_REQUEST_MILLICORES) {
            return Some(
                self.refuse_and_delete(
                    permit,
                    observed,
                    RefusalReason::CeilingBelowLauncherRequest {
                        ceiling,
                        request: u64::from(LAUNCHER_CPU_REQUEST_MILLICORES),
                    },
                )
                .await,
            );
        }
        if ceiling < BIRTH_CPU_MILLICORES {
            return Some(
                self.refuse_and_delete(
                    permit,
                    observed,
                    RefusalReason::CeilingBelowBirthLimit {
                        ceiling,
                        birth: BIRTH_CPU_MILLICORES,
                    },
                )
                .await,
            );
        }
        None
    }

    /// The write-once capture. Every non-success is classified here, because
    /// the three failure shapes have three different correct responses.
    async fn capture(
        &self,
        permit: &PermitBinding,
        observed: &ObservedLauncherSidecar,
        effective: LauncherAuthorityProtocol,
    ) -> Result<BuildPodPermitRow, BootstrapOutcome> {
        let (Some(launcher_container_id), Some(image_digest), Some(observed_protocol)) = (
            observed.launcher_container_id.clone(),
            observed.image_digest.clone(),
            observed.observed_protocol.clone(),
        ) else {
            return Err(BootstrapOutcome::NotReady(NotReady::LauncherNotStarted {
                field: if observed.launcher_container_id.is_none() {
                    "status.initContainerStatuses[].containerID"
                } else {
                    "status.initContainerStatuses[].imageID"
                },
            }));
        };
        let Some(ceiling) = observed.admitted_cpu_millicores else {
            return Err(BootstrapOutcome::Refused {
                reason: RefusalReason::NoAdmittedCeiling,
                disposition: PodDisposition::Retained,
            });
        };
        let Ok(admitted_cpu_millicores) = i64::try_from(ceiling) else {
            return Err(self
                .refuse_and_delete(permit, observed, RefusalReason::NoAdmittedCeiling)
                .await);
        };

        let identity = BuildPodResizeIdentity {
            pod_namespace: observed.namespace.clone(),
            pod_name: observed.pod_name.clone(),
            pod_uid: observed.pod_uid.clone(),
            launcher_container_name: observed.launcher_container_name.clone(),
            launcher_container_id,
            image_digest,
            observed_launcher_protocol: observed_protocol,
            effective_launcher_protocol: effective.as_wire().to_owned(),
            admitted_cpu_millicores,
        };

        match self
            .permits
            .capture_resize_identity(
                &permit.task_run_id,
                &permit.permit_id,
                permit.fencing_token,
                &identity,
            )
            .await
        {
            Ok(
                CaptureBuildPodResizeIdentityResult::Captured(row)
                | CaptureBuildPodResizeIdentityResult::AlreadyCaptured(row),
            ) => Ok(row),
            Ok(CaptureBuildPodResizeIdentityResult::Rejected) => Err(self
                .refuse_and_delete(permit, observed, RefusalReason::ConflictingCapture)
                .await),
            Err(error) if is_unique_violation(&error) => Err(self
                .refuse_and_delete(
                    permit,
                    observed,
                    RefusalReason::PodUidClaimedByAnotherPermit,
                )
                .await),
            Err(error) => Err(BootstrapOutcome::NotReady(NotReady::StorageUnavailable(
                error.to_string(),
            ))),
        }
    }

    /// Give up on a Pod that never became governable, and destroy it.
    ///
    /// The caller reaches this when its wait budget is exhausted — every
    /// [`NotReady`] variant is retryable, so "still not ready" eventually has to
    /// become a decision. The decision is the same one
    /// [`Self::refuse_and_delete`] makes and for the same reason: the bootstrap
    /// window sits entirely before dispatch, so no repository-controlled command
    /// has run, and a Pod whose quota authority was never established must not
    /// be left alive.
    ///
    /// Returns [`PodDisposition::Retained`] when there is nothing to fence a
    /// delete to — no Pod, or a launcher that cannot be uniquely named, in which
    /// case the observation carries no Pod UID at all. The caller's own Job
    /// teardown remains the backstop for that case; guessing a UID is not an
    /// option, because an unfenced delete can destroy a *replacement* Pod that
    /// some other actor legitimately owns.
    pub async fn abandon(&self, permit: &PermitBinding, reason: &str) -> PodDisposition {
        let observed = match self.surface.observe_launcher(&permit.task_run_id).await {
            Ok(Some(observed)) => observed,
            Ok(None) | Err(_) => {
                warn!(
                    task_run_id = %permit.task_run_id,
                    reason,
                    "task_run_resize_bootstrap: abandoning dispatch; no fenceable Pod to delete"
                );
                return PodDisposition::Retained;
            }
        };
        let deleted = self
            .surface
            .uid_fenced_delete(&permit.task_run_id, &observed.pod_uid)
            .await;
        warn!(
            task_run_id = %permit.task_run_id,
            pod_uid = %observed.pod_uid,
            reason,
            delete_failed = deleted.is_err(),
            "task_run_resize_bootstrap: abandoning dispatch and UID-fenced deleting the Pod"
        );
        PodDisposition::UidFencedDeleted(deleted)
    }

    /// Fail closed: UID-fenced delete exactly the observed Pod, then refuse.
    ///
    /// Deletion is the fail-safe because no repository-controlled command has
    /// run yet — the bootstrap window sits entirely before dispatch — so the
    /// cost of destroying the Pod is one dispatch, while the cost of keeping it
    /// is a live Pod whose quota nobody has authority over.
    async fn refuse_and_delete(
        &self,
        permit: &PermitBinding,
        observed: &ObservedLauncherSidecar,
        reason: RefusalReason,
    ) -> BootstrapOutcome {
        let deleted = self
            .surface
            .uid_fenced_delete(&permit.task_run_id, &observed.pod_uid)
            .await;
        warn!(
            task_run_id = %permit.task_run_id,
            pod_uid = %observed.pod_uid,
            %reason,
            delete_failed = deleted.is_err(),
            "task_run_resize_bootstrap: refusing dispatch and UID-fenced deleting the Pod"
        );
        BootstrapOutcome::Refused {
            reason,
            disposition: PodDisposition::UidFencedDeleted(deleted),
        }
    }
}

// ── The production composition (0ppk-1b) ───────────────────────────────────
//
// Everything above this line was reachable only from tests. What follows is the
// object `AppState` builds at boot and the dispatch seam calls: it turns one
// bootstrap *pass* into one dispatch *decision* by driving passes until the
// launcher is confirmed, the bootstrap refuses, or a wait budget runs out.

/// Environment override for the birth-confirmation wait budget, in seconds.
pub const BIRTH_ADMISSION_BUDGET_ENV: &str = "DJINN_BIRTH_CONFIRMATION_BUDGET_SECS";

/// How long one dispatch waits for its launcher sidecar to be admitted and its
/// birth downsize confirmed.
///
/// Deliberately shorter than the supervisor's 480s pre-session deadline: an
/// unconfirmable Pod must surface as a dispatch failure that names *why*, not as
/// a pre-session timeout that names nothing.
pub const BIRTH_ADMISSION_BUDGET_DEFAULT: Duration = Duration::from_secs(180);

/// Interval between bootstrap passes while the launcher is still starting.
const BIRTH_ADMISSION_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Where the bridge gets its apiserver surface.
enum SurfaceSource {
    /// Built once, lazily, from ambient cluster configuration. Lazy because the
    /// composition root is synchronous and building a `kube::Client` is not —
    /// and because a server that never dispatches a Kubernetes Job should not
    /// fail to boot for want of a kubeconfig.
    Ambient(tokio::sync::OnceCell<Arc<dyn TaskRunPodSurface>>),
    /// Supplied by the caller. Fixtures use this; so would any future caller
    /// that already holds a live surface.
    Fixed(Arc<dyn TaskRunPodSurface>),
}

/// The server's resize stack, as the dispatch seam consumes it.
///
/// One gate is shared across every dispatch this bridge serves, deliberately:
/// the unadmitted-dispatch counter is a process-wide statement about ordering,
/// and a per-dispatch gate could not make it.
pub struct TaskRunResizeAdmissionBridge {
    db: djinn_db::Database,
    gate: Arc<DispatchGate>,
    surface: SurfaceSource,
    budget: Duration,
    poll_interval: Duration,
}

impl TaskRunResizeAdmissionBridge {
    /// The production constructor: durable permits over `db`, and an apiserver
    /// surface resolved from ambient cluster configuration on first use.
    #[must_use]
    pub fn from_env(db: djinn_db::Database) -> Self {
        Self {
            db,
            gate: Arc::new(DispatchGate::new()),
            surface: SurfaceSource::Ambient(tokio::sync::OnceCell::new()),
            budget: budget_from_env(),
            poll_interval: BIRTH_ADMISSION_POLL_INTERVAL,
        }
    }

    /// Same composition against a caller-supplied surface.
    #[must_use]
    pub fn with_surface(db: djinn_db::Database, surface: Arc<dyn TaskRunPodSurface>) -> Self {
        Self {
            db,
            gate: Arc::new(DispatchGate::new()),
            surface: SurfaceSource::Fixed(surface),
            budget: budget_from_env(),
            poll_interval: BIRTH_ADMISSION_POLL_INTERVAL,
        }
    }

    /// Override the wait budget and poll interval.
    #[must_use]
    pub const fn with_wait(mut self, budget: Duration, poll_interval: Duration) -> Self {
        self.budget = budget;
        self.poll_interval = poll_interval;
        self
    }

    /// The shared gate. Exposed so the composition root and its tests can read
    /// the unadmitted-dispatch counter.
    #[must_use]
    pub const fn gate(&self) -> &Arc<DispatchGate> {
        &self.gate
    }

    async fn surface(&self) -> Result<Arc<dyn TaskRunPodSurface>, String> {
        match &self.surface {
            SurfaceSource::Fixed(surface) => Ok(surface.clone()),
            SurfaceSource::Ambient(cell) => cell
                .get_or_try_init(|| async {
                    djinn_k8s::runtime::TaskRunPodResizeSurface::from_env()
                        .await
                        .map(|surface| Arc::new(surface) as Arc<dyn TaskRunPodSurface>)
                })
                .await
                .cloned(),
        }
    }
}

fn budget_from_env() -> Duration {
    std::env::var(BIRTH_ADMISSION_BUDGET_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(BIRTH_ADMISSION_BUDGET_DEFAULT, Duration::from_secs)
}

fn refused(reason: impl Into<String>, disposition: &PodDisposition) -> ResizeAdmissionRefused {
    ResizeAdmissionRefused {
        reason: reason.into(),
        pod_deleted: matches!(disposition, PodDisposition::UidFencedDeleted(Ok(()))),
    }
}

#[async_trait]
impl TaskRunResizeAdmission for TaskRunResizeAdmissionBridge {
    async fn admit_dispatch(
        &self,
        request: &ResizeAdmissionRequest,
    ) -> Result<ResizeAdmissionOutcome, ResizeAdmissionRefused> {
        let surface = self.surface().await.map_err(|error| ResizeAdmissionRefused {
            reason: format!("no apiserver surface for the resize bootstrap: {error}"),
            pod_deleted: false,
        })?;
        let permit = PermitBinding {
            task_run_id: request.task_run_id.clone(),
            permit_id: request.permit_id.clone(),
            fencing_token: request.fencing_token,
        };
        let bootstrap = TaskRunResizeBootstrap::new(
            BuildPodPermitRepository::new(self.db.clone()),
            surface,
            self.gate.clone(),
        );

        let deadline = tokio::time::Instant::now() + self.budget;
        // Assigned on exactly the path that breaks out of the loop, so the
        // refusal below always names the condition that was still unmet.
        let stalled: String;
        loop {
            match bootstrap.bootstrap(&permit, request.effective_protocol).await {
                BootstrapOutcome::BirthConfirmed {
                    pod_uid,
                    admitted_cpu_millicores,
                } => {
                    // The gate, not the outcome, is what admits. It is keyed on
                    // the in-process confirmed result of this very bootstrap, so
                    // a `birth_confirmed` row left behind by a previous process
                    // cannot admit anything on its own.
                    self.gate
                        .admit(&permit.task_run_id)
                        .map_err(|error| ResizeAdmissionRefused {
                            reason: error.to_string(),
                            pod_deleted: false,
                        })?;
                    return Ok(ResizeAdmissionOutcome::BirthConfirmed {
                        pod_uid,
                        admitted_cpu_millicores,
                    });
                }
                BootstrapOutcome::LeafAuthority => {
                    self.gate
                        .admit(&permit.task_run_id)
                        .map_err(|error| ResizeAdmissionRefused {
                            reason: error.to_string(),
                            pod_deleted: false,
                        })?;
                    return Ok(ResizeAdmissionOutcome::LeafAuthority);
                }
                BootstrapOutcome::NotReady(reason) => {
                    if tokio::time::Instant::now() >= deadline {
                        stalled = reason.to_string();
                        break;
                    }
                    tokio::time::sleep(self.poll_interval).await;
                }
                BootstrapOutcome::Refused {
                    reason,
                    disposition,
                } => {
                    return Err(refused(reason.to_string(), &disposition));
                }
            }
        }

        let message = format!(
            "launcher birth limit was not confirmed within {}s: {stalled}",
            self.budget.as_secs()
        );
        let disposition = bootstrap.abandon(&permit, &message).await;
        Err(refused(message, &disposition))
    }

    fn record_dispatch_started(&self, task_run_id: &str) {
        self.gate.record_dispatch_started(task_run_id);
    }
}

/// Whether a durable error is the unique-violation raised by
/// `build_pod_permits_pod_uid_key`.
fn is_unique_violation(error: &DbError) -> bool {
    let DbError::Sqlx(error) = error else {
        return false;
    };
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == UNIQUE_VIOLATION)
}

#[cfg(test)]
#[path = "task_run_resize_bootstrap_tests.rs"]
mod tests;
