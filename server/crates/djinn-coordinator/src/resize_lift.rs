//! The status-confirmed lift: every fence `PodResizeClient` does not have.
//!
//! [`crate::resize_authorization::ResizeAuthority`] decides *whether* a resize is
//! allowed and *what* the clamped target is, from durable rows alone. This module
//! is what actually moves the Pod, and it is where an authorized intent becomes
//! either a lease grant or a settled degrade.
//!
//! # What `PodResizeClient::resize_launcher_cpu` does NOT do
//!
//! Its whole signature is `(&self, pod_name: &str, target: CpuLimit)`. From that
//! alone, four fences are missing and every one of them is this module's job:
//!
//! 1. **No UID fence.** It never reads `metadata.uid`. A Pod deleted and
//!    recreated under the same name is, to it, the same Pod. We compare the live
//!    `metadata.uid` against the permit's write-once `pod_uid` on the fresh GET
//!    **before** any PATCH, and again after confirmation.
//! 2. **No restart fence.** The permit captured
//!    `status.initContainerStatuses[..].containerID`. A launcher that restarted
//!    is a different cgroup than the one the lift was reasoned about.
//! 3. **No protocol fence.** The permit captured
//!    `effective_launcher_protocol`. Exactly one authority governs an admitted
//!    Pod; a live Pod that declares another one is a hard failure.
//! 4. **No polling.** It performs exactly one GET / PATCH / GET cycle.
//!    `status.initContainerStatuses` is populated asynchronously, so a single
//!    immediate confirming GET usually reads stale. The bounded confirmation
//!    poll lives here, because the deadline has to have exactly one owner.
//!
//! What it *does* get right is reused verbatim and never re-derived here:
//! `Patch::Strategic` (because `initContainers` carries `patchMergeKey: name`,
//! and a Merge patch replaces the whole array — proven live in #2861, where the
//! apiserver answered `spec.initContainers[0].resources.limits: Forbidden:
//! resource limits cannot be removed`), the refusal to ever consult
//! `spec.containers` or `status.containerStatuses`, the `PodResizePending`
//! fail-closed, and the **millicore** comparison. That last one is not
//! defensive: #2861 observed the apiserver canonicalise `2000m` to `2` and a
//! string comparison report `never reported 2000m; last observed Some(2000)`.
//!
//! # Everything degrades. Nothing errors.
//!
//! [`PodResizeApplier::apply`] returns `Result<(), ResizeApplyFailure>`, and
//! `fold_into_grant` maps every failure onto
//! [`LeaseResult::DegradedUnleased`](djinn_supervisor::services::LeaseResult) —
//! never onto an `Err` the invocation runner would classify as retryable. Every
//! input to a lift is settled: which Pod the permit was captured against, what
//! ceiling was admitted, whether the kubelet actuated. Re-asking spends the
//! queue deadline on a question whose answer is fixed while the child runs
//! clamped the whole time.
//!
//! # A failed lift leaves the permit `drop_required`
//!
//! Before the PATCH the lifecycle moves `birth_confirmed → lift_applying`; on a
//! confirmed lift, `lift_applying → lifted`; on **any** failure after that
//! transition, `→ drop_required`. Migration 164's trigger owns the legal edge
//! list, so this module cannot invent one. A lift that could not be confirmed
//! must never leave a row claiming `lifted` — the drop reconciler (`0ppk-3`) is
//! what returns the Pod to its birth limit, and it reads `drop_required`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_db::{
    BuildPodPermitRepository, BuildPodPermitState, TransitionBuildPodResizeLifecycleResult,
};
use djinn_k8s::pod_resize::{NotConfirmed, PodResizeError};
use djinn_k8s::runtime::{
    LauncherObservationError, ObservedLauncherSidecar, TaskRunPodResizeSurface,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_supervisor::services::DegradedUnleasedReason;

use crate::resize_authorization::{PodResizeApplier, PodResizeIntent, ResizeApplyFailure};

/// How long a lift waits for `status.initContainerStatuses` to agree.
///
/// The kubelet actuates asynchronously, so a confirmation budget of zero would
/// degrade almost every healthy lift. It is bounded well below the invocation's
/// own queue deadline: a lift that has not actuated in this long is not going
/// to, and the invocation is better off running unleased than waiting.
pub const LIFT_CONFIRMATION_BUDGET: Duration = Duration::from_secs(30);

/// Interval between confirmation observations.
pub const LIFT_CONFIRMATION_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The two apiserver operations a lift performs.
///
/// Behind a trait so the whole fenced sequence is drivable from fixtures with no
/// cluster, and so this crate needs no `k8s-openapi` dependency of its own — the
/// two types crossing this boundary are `djinn-k8s`'s own flattened
/// [`ObservedLauncherSidecar`] and [`PodResizeError`], never a `Pod`.
///
/// It is deliberately the *same shape* as `djinn-server`'s `TaskRunPodSurface`
/// minus the fenced delete, rather than a reuse of it: `djinn-server` depends on
/// this crate, so the trait cannot live there.
#[async_trait]
pub trait LauncherResizeSurface: Send + Sync {
    /// Fresh, label-scoped GET of the single Pod for this task run, flattened to
    /// the launcher facts every fence is compared against. `Ok(None)` means no
    /// Pod carries the label.
    ///
    /// # Errors
    ///
    /// [`LauncherObservationError`] when the read fails, the Pod is not fully
    /// admitted, or the launcher cannot be uniquely named.
    async fn observe_launcher(
        &self,
        task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError>;

    /// One limits-only `pods/resize` PATCH of the launcher sidecar, confirmed
    /// through `status.initContainerStatuses`.
    ///
    /// # Errors
    ///
    /// [`PodResizeError`]; `NotConfirmed` when the PATCH was accepted but the
    /// fresh init-container status does not yet agree.
    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError>;
}

#[async_trait]
impl LauncherResizeSurface for TaskRunPodResizeSurface {
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
}

/// Where the lift gets its apiserver surface.
enum SurfaceSource {
    /// Built once, lazily, from ambient cluster configuration. Lazy because the
    /// composition root is synchronous and building a `kube::Client` is not, and
    /// because a server with no kubeconfig must still boot. A server that never
    /// resolves one degrades every lift to
    /// [`DegradedUnleasedReason::ResizeSurfaceUnavailable`] — which is exactly
    /// the fail-closed answer, and is never a grant.
    Ambient(tokio::sync::OnceCell<Arc<dyn LauncherResizeSurface>>),
    /// Supplied by the caller. Fixtures use this.
    Fixed(Arc<dyn LauncherResizeSurface>),
}

/// The status-confirmed, UID/restart/protocol-fenced lift.
pub struct ResizeLift {
    permits: Arc<BuildPodPermitRepository>,
    surface: SurfaceSource,
    budget: Duration,
    poll_interval: Duration,
}

impl ResizeLift {
    /// The production constructor: durable permits over `permits`, apiserver
    /// surface resolved from ambient cluster configuration on first lift.
    #[must_use]
    pub fn from_env(permits: Arc<BuildPodPermitRepository>) -> Self {
        Self {
            permits,
            surface: SurfaceSource::Ambient(tokio::sync::OnceCell::new()),
            budget: LIFT_CONFIRMATION_BUDGET,
            poll_interval: LIFT_CONFIRMATION_POLL_INTERVAL,
        }
    }

    /// Same composition against a caller-supplied surface.
    #[must_use]
    pub fn with_surface(
        permits: Arc<BuildPodPermitRepository>,
        surface: Arc<dyn LauncherResizeSurface>,
    ) -> Self {
        Self {
            permits,
            surface: SurfaceSource::Fixed(surface),
            budget: LIFT_CONFIRMATION_BUDGET,
            poll_interval: LIFT_CONFIRMATION_POLL_INTERVAL,
        }
    }

    /// Override the confirmation budget and poll interval.
    ///
    /// A zero budget means "one observation, no waiting", which is what lets a
    /// test see the *specific* thing an unconfirmed status reported rather than
    /// the fact that a budget was spent. See
    /// [`DegradedUnleasedReason::LiftDeadlineExceeded`].
    #[must_use]
    pub const fn with_wait(mut self, budget: Duration, poll_interval: Duration) -> Self {
        self.budget = budget;
        self.poll_interval = poll_interval;
        self
    }

    async fn surface(&self) -> Result<Arc<dyn LauncherResizeSurface>, ResizeApplyFailure> {
        match &self.surface {
            SurfaceSource::Fixed(surface) => Ok(Arc::clone(surface)),
            SurfaceSource::Ambient(cell) => cell
                .get_or_try_init(|| async {
                    TaskRunPodResizeSurface::from_env()
                        .await
                        .map(|surface| Arc::new(surface) as Arc<dyn LauncherResizeSurface>)
                })
                .await
                .cloned()
                .map_err(|error| {
                    ResizeApplyFailure::new(
                        DegradedUnleasedReason::ResizeSurfaceUnavailable,
                        format!("no apiserver surface for the resize lift: {error}"),
                    )
                }),
        }
    }

    /// Claim the lifecycle for this invocation: `birth_confirmed →
    /// lift_applying`, writing the invocation fence.
    ///
    /// A second, concurrent invocation of the same task run loses here without a
    /// write and therefore without a PATCH — see
    /// [`BuildPodPermitRepository::begin_resize_invocation`].
    async fn begin_invocation(&self, intent: &PodResizeIntent) -> Result<(), ResizeApplyFailure> {
        match self
            .permits
            .begin_resize_invocation(
                &intent.task_run_id,
                &intent.permit_id,
                intent.fencing_token,
                &intent.pod_uid,
                &intent.invocation_id,
            )
            .await
        {
            Ok(TransitionBuildPodResizeLifecycleResult::Transitioned(_)) => Ok(()),
            Ok(TransitionBuildPodResizeLifecycleResult::Rejected) => Err(ResizeApplyFailure::new(
                DegradedUnleasedReason::LiftLifecycleUnwritable,
                format!(
                    "permit {} refused invocation {}'s lift claim; another invocation \
                     owns this lifecycle",
                    intent.permit_id, intent.invocation_id
                ),
            )),
            Err(error) => Err(ResizeApplyFailure::new(
                DegradedUnleasedReason::LiftLifecycleUnwritable,
                format!("durable permit lifecycle unwritable: {error}"),
            )),
        }
    }

    /// Compare-and-swap one lifecycle edge, fenced on all four permit fields
    /// **and** on the owning invocation.
    async fn transition(
        &self,
        intent: &PodResizeIntent,
        expected: BuildPodPermitState,
        next: BuildPodPermitState,
    ) -> Result<(), ResizeApplyFailure> {
        match self
            .permits
            .transition_resize_lifecycle(
                &intent.task_run_id,
                &intent.permit_id,
                intent.fencing_token,
                &intent.pod_uid,
                Some(&intent.invocation_id),
                expected,
                next,
            )
            .await
        {
            Ok(TransitionBuildPodResizeLifecycleResult::Transitioned(_)) => Ok(()),
            Ok(TransitionBuildPodResizeLifecycleResult::Rejected) => Err(ResizeApplyFailure::new(
                DegradedUnleasedReason::LiftLifecycleUnwritable,
                format!(
                    "permit {} refused the {expected:?} -> {next:?} transition; \
                         another actor owns this lifecycle",
                    intent.permit_id
                ),
            )),
            Err(error) => Err(ResizeApplyFailure::new(
                DegradedUnleasedReason::LiftLifecycleUnwritable,
                format!("durable permit lifecycle unwritable: {error}"),
            )),
        }
    }

    /// Best-effort `→ drop_required`.
    ///
    /// The lift has already failed by the time this runs, so its own failure
    /// cannot change the reason the caller sees — the Pod is left holding a
    /// limit nobody granted a lease for either way, and a reconciler that finds
    /// a stranded `lift_applying` row treats it the same as `drop_required`.
    /// What must not happen is the failure being swallowed silently, so it is
    /// logged at `error`.
    async fn require_drop(&self, intent: &PodResizeIntent, from: BuildPodPermitState) {
        if let Err(failure) = self
            .transition(intent, from, BuildPodPermitState::DropRequired)
            .await
        {
            tracing::error!(
                task_run_id = %intent.task_run_id,
                permit_id = %intent.permit_id,
                detail = %failure.detail,
                "resize lift: could not mark the permit drop-required after a failed lift"
            );
        }
    }

    /// Fresh observation plus all three identity fences. **No PATCH is issued
    /// from here**, which is what makes the recreated-Pod case assertable on a
    /// PATCH counter of zero.
    async fn observe_and_fence(
        &self,
        surface: &Arc<dyn LauncherResizeSurface>,
        intent: &PodResizeIntent,
    ) -> Result<ObservedLauncherSidecar, ResizeApplyFailure> {
        let observed = match surface.observe_launcher(&intent.task_run_id).await {
            Ok(Some(observed)) => observed,
            Ok(None) => {
                return Err(ResizeApplyFailure::new(
                    DegradedUnleasedReason::LiftPodAbsent,
                    format!(
                        "no Pod carries the label for task run {}",
                        intent.task_run_id
                    ),
                ));
            }
            Err(error) => return Err(observation_failure(&error)),
        };

        // THE UID FENCE. `resize_launcher_cpu` takes only a name; this is the
        // only thing standing between a recreated Pod and a PATCH addressed to
        // it. Removing this comparison is acceptance criterion 5's named
        // mutation, and the recreated-Pod test then observes PATCH count 1.
        if observed.pod_uid != intent.pod_uid {
            return Err(ResizeApplyFailure::new(
                DegradedUnleasedReason::ResizeIdentityChanged,
                format!(
                    "Pod `{}` is now uid `{}`; the permit was captured against `{}`",
                    intent.pod_name, observed.pod_uid, intent.pod_uid
                ),
            ));
        }

        // THE PROTOCOL FENCE. Checked before the restart fence deliberately: a
        // Pod running a different authority is a mis-governance fact about the
        // whole object, while a restart is a fact about one container.
        let declared = observed
            .observed_protocol
            .as_deref()
            .map(str::parse::<LauncherAuthorityProtocol>);
        match declared {
            Some(Ok(protocol)) if protocol == intent.effective_launcher_protocol => {}
            other => {
                return Err(ResizeApplyFailure::new(
                    DegradedUnleasedReason::LauncherProtocolChanged,
                    format!(
                        "launcher declares protocol {:?}; the permit resolved `{}`",
                        other.map(|parsed| parsed.map(|p| p.as_wire().to_owned())),
                        intent.effective_launcher_protocol.as_wire()
                    ),
                ));
            }
        }

        // THE RESTART FENCE.
        match observed.launcher_container_id.as_deref() {
            Some(live) if live == intent.launcher_container_id => {}
            live => {
                return Err(ResizeApplyFailure::new(
                    DegradedUnleasedReason::LauncherRestarted,
                    format!(
                        "launcher containerID is now {live:?}; the permit captured `{}`",
                        intent.launcher_container_id
                    ),
                ));
            }
        }

        Ok(observed)
    }

    /// PATCH, then poll `status.initContainerStatuses` until it agrees or the
    /// budget is gone, re-fencing on every observation.
    async fn patch_and_confirm(
        &self,
        surface: &Arc<dyn LauncherResizeSurface>,
        intent: &PodResizeIntent,
    ) -> Result<(), ResizeApplyFailure> {
        let target = u64::try_from(intent.target_millicores).map_err(|_| {
            ResizeApplyFailure::new(
                DegradedUnleasedReason::CeilingUnusable,
                format!(
                    "clamped target {}m is not a CPU quantity",
                    intent.target_millicores
                ),
            )
        })?;

        let deadline = tokio::time::Instant::now() + self.budget;
        let mut waited = false;
        loop {
            // Each pass re-runs the full identity fence before touching the
            // Pod. A launcher that restarts, or a Pod that is replaced, *during*
            // the confirmation poll must stop the lift rather than have the next
            // PATCH land on a different object.
            self.observe_and_fence(surface, intent).await?;

            match surface.resize_launcher_cpu(&intent.pod_name, target).await {
                Ok(()) => {
                    // The confirming fence. `resize_launcher_cpu`'s own final
                    // GET proves the LIMIT; it does not prove the limit belongs
                    // to the object we authorized. Only re-reading the UID and
                    // the container ID does that.
                    self.observe_and_fence(surface, intent).await?;
                    return Ok(());
                }
                Err(error) => {
                    let failure = apply_failure(&error);
                    if !is_retryable(&error) {
                        return Err(failure);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        // Budget already gone. Report the specific thing the
                        // last observation saw — "the kubelet has not moved" is
                        // more useful than "time passed".
                        return Err(if waited {
                            ResizeApplyFailure::new(
                                DegradedUnleasedReason::LiftDeadlineExceeded,
                                format!(
                                    "confirmation budget of {:?} spent; last: {}",
                                    self.budget, failure.detail
                                ),
                            )
                        } else {
                            failure
                        });
                    }
                    waited = true;
                    tokio::time::sleep(self.poll_interval).await;
                }
            }
        }
    }
}

#[async_trait]
impl PodResizeApplier for ResizeLift {
    async fn apply(&self, intent: &PodResizeIntent) -> Result<(), ResizeApplyFailure> {
        // The lifecycle edge is chosen from the permit state the authorization
        // read. `birth_confirmed` is a fresh lift; `lifted` is an idempotent
        // re-confirmation of one that already happened (a worker may present the
        // same grant more than once, and migration 164 has no `lifted ->
        // lift_applying` edge). Anything else is not a liftable subject.
        let entered = match intent.permit_state {
            BuildPodPermitState::BirthConfirmed => {
                self.begin_invocation(intent).await?;
                BuildPodPermitState::LiftApplying
            }
            BuildPodPermitState::Lifted => {
                // A self-transition, purely to run the invocation fence BEFORE
                // any PATCH. Without it a second invocation that authorized
                // while the first already held `lifted` would re-confirm a lift
                // it does not own — and, because the `Lifted` arm skips the
                // closing transition below, it would never be fenced at all.
                // The lifecycle is one row per task run and nothing serializes
                // invocations of one run, so this is a reachable race, not a
                // hypothetical one.
                self.transition(
                    intent,
                    BuildPodPermitState::Lifted,
                    BuildPodPermitState::Lifted,
                )
                .await?;
                BuildPodPermitState::Lifted
            }
            other => {
                return Err(ResizeApplyFailure::new(
                    DegradedUnleasedReason::PermitNotLiftable,
                    format!("permit is in {other:?}; a lift may only start from birth_confirmed"),
                ));
            }
        };

        let surface = match self.surface().await {
            Ok(surface) => surface,
            Err(failure) => {
                self.require_drop(intent, entered).await;
                return Err(failure);
            }
        };

        match self.patch_and_confirm(&surface, intent).await {
            Ok(()) => {
                if entered == BuildPodPermitState::LiftApplying
                    && let Err(failure) = self
                        .transition(
                            intent,
                            BuildPodPermitState::LiftApplying,
                            BuildPodPermitState::Lifted,
                        )
                        .await
                {
                    // The Pod moved but the ledger did not. Refusing the
                    // grant here is the only answer that keeps the two in
                    // agreement: a row that does not say `lifted` will be
                    // dropped back to the birth limit by the reconciler, and
                    // a lease granted against it would be a lease for CPU
                    // that is about to be taken away.
                    self.require_drop(intent, BuildPodPermitState::LiftApplying)
                        .await;
                    return Err(failure);
                }
                Ok(())
            }
            Err(failure) => {
                self.require_drop(intent, entered).await;
                Err(failure)
            }
        }
    }
}

/// Whether waiting could still change this answer.
///
/// Only an unconfirmed *status* is retryable: the kubelet actuates
/// asynchronously. An apiserver verdict, an identity ambiguity or an unparseable
/// quantity is settled, and re-asking spends the budget for nothing.
const fn is_retryable(error: &PodResizeError) -> bool {
    matches!(
        error,
        PodResizeError::NotConfirmed(
            NotConfirmed::ResizePending
                | NotConfirmed::StatusLimitAbsent
                | NotConfirmed::StatusStale { .. }
        )
    )
}

/// Map one `pods/resize` failure onto its settled reason.
fn apply_failure(error: &PodResizeError) -> ResizeApplyFailure {
    let reason = match error {
        PodResizeError::LauncherIdentityAmbiguous { .. } => {
            DegradedUnleasedReason::LiftIdentityAmbiguous
        }
        PodResizeError::InvalidCpuQuantity { .. } => DegradedUnleasedReason::CeilingUnusable,
        PodResizeError::NotConfirmed(NotConfirmed::ResizePending) => {
            DegradedUnleasedReason::LiftResizePending
        }
        PodResizeError::NotConfirmed(NotConfirmed::StatusLimitAbsent) => {
            DegradedUnleasedReason::LiftStatusAbsent
        }
        PodResizeError::NotConfirmed(
            NotConfirmed::StatusStale { .. } | NotConfirmed::StatusLimitUnparseable { .. },
        ) => DegradedUnleasedReason::LiftStatusStale,
        // Classified on the NUMERIC status, never on the rendered message: a
        // missing `pods/resize` RBAC rule (403) is an operator fact, a rejected
        // resize (422) is a request fact, and a transport failure knows nothing
        // at all. See `PodResizeError::Api::status`.
        PodResizeError::Api { status, .. } => match status {
            Some(403) => DegradedUnleasedReason::ResizeForbidden,
            Some(422) => DegradedUnleasedReason::ResizeRejected,
            _ => DegradedUnleasedReason::ResizeSurfaceUnavailable,
        },
    };
    ResizeApplyFailure::new(reason, error.to_string())
}

/// Map one observation failure onto its settled reason.
fn observation_failure(error: &LauncherObservationError) -> ResizeApplyFailure {
    let reason = match error {
        // An incomplete `metadata` is the same answer as no Pod: there is
        // nothing to fence a resize against.
        LauncherObservationError::Incomplete { .. } => DegradedUnleasedReason::LiftPodAbsent,
        LauncherObservationError::Ambiguous(inner) => return apply_failure(inner),
        LauncherObservationError::Api(_) => DegradedUnleasedReason::ResizeSurfaceUnavailable,
    };
    ResizeApplyFailure::new(reason, error.to_string())
}
