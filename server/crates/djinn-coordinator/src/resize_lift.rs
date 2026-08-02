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
//! 5. **No ceiling.** It sends whatever millicores it is handed. The bound —
//!    the ceiling the Pod was actually admitted with — is applied to the value
//!    about to go on the wire by [`clamp_to_admitted_ceiling`], so no caller
//!    that can build a [`PodResizeIntent`] can talk it into an over-ceiling
//!    PATCH.
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
//!
//! # …and when even THAT write is refused, the lift is undone on the wire
//!
//! `→ drop_required` is itself a compare-and-swap, and it can lose. Measured in
//! production on 2026-08-01, 2m42s after a lift failure at 19:18:13.470Z:
//!
//! ```text
//! permit row : state=birth_confirmed  resize_invocation_id=019fbec3-2047-…  admitted=4000
//! live Pod   : SPEC   init cgroup-launcher limits.cpu = "4"
//!              STATUS init cgroup-launcher limits.cpu = "4"
//! ```
//!
//! The 4000m PATCH had already landed AND been status-confirmed; the closing
//! `lift_applying → lifted` CAS was refused because the in-process 30s drop
//! reconciler had walked the same row `lift_applying → drop_required →
//! drop_applying → birth_confirmed` underneath it; and the fallback
//! `lift_applying → drop_required` was refused for the same reason and then
//! **only logged**. The ledger said birth-clamped, the kubelet said 4 cores, and
//! nothing reconciled it: `strandedness()` classifies `birth_confirmed` with a
//! live owner as `Live` and deliberately will not touch it. Eleven lift failures
//! in one session — roughly 26% of lifts — all of this shape.
//!
//! So a refused closing transition **actuates**. [`ResizeLift::require_drop`]
//! re-reads the durable row, and when the ledger is not claiming a lift for this
//! Pod it returns the launcher to [`BIRTH_CPU_MILLICORES`] with a real,
//! status-confirmed PATCH. The invariant that restores is the one the log line
//! `resize lift did not take; degrading unleased` has always implied and never
//! enforced: **the launcher's actual limit may not exceed what the ledger says
//! was admitted for it.**
//!
//! It has to be safe under the very race that produced the defect, because the
//! competing actor is a reconciler ticking every 30 seconds against the same
//! row. So the fallback re-reads before acting, refuses to touch a row another
//! driver owns or one that durably records a lift, treats "already back at
//! birth" as success with **zero** PATCHes, and fences the Pod UID on every
//! observation.

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

/// The CPU limit every `resize-v2` launcher sidecar is downsized to before its
/// worker session may dispatch — and therefore the limit a lift that could not
/// be recorded has to put back.
///
/// Restated from `djinn_server::task_run_resize_bootstrap::BIRTH_CPU_MILLICORES`
/// rather than imported, because `djinn-server` depends on this crate and not
/// the other way round. It is NOT a second source of truth: that module carries
/// a `const _: () = assert!(…)` against this constant, so the two numbers
/// drifting apart is a build failure rather than a Pod left holding CPU its
/// ledger does not admit.
pub const BIRTH_CPU_MILLICORES: u64 = 250;

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

/// What the durable permit row claims about a launcher's CPU limit, re-read at
/// the moment a losing lift has to decide whether to undo itself.
///
/// The whole point of this type is that the answer is NOT "what state did this
/// invocation leave the row in" — by the time it is consulted, the row has
/// demonstrably moved under us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LedgerClaim {
    /// The row durably records a lift for this Pod. The launcher holding the
    /// admitted ceiling is exactly what the ledger says, so nothing is owed.
    Lifted,
    /// Another invocation, or another Pod's permit, owns this lifecycle. It
    /// settles its own row; a PATCH from here would clamp somebody else's lift.
    OwnedByAnotherDriver,
    /// The ledger claims the birth clamp. A launcher above it is a strand.
    ///
    /// `state` is the row's state as re-read, carried so a failed actuation can
    /// still attempt the durable hand-off to the drop reconciler from the state
    /// the row is *actually* in. `None` means no row was readable at all, and
    /// therefore no hand-off is possible.
    BirthClamped { state: Option<BuildPodPermitState> },
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

    /// `→ drop_required`, and — when that write is refused — the PATCH that
    /// makes the refusal true anyway.
    ///
    /// The lift has already failed by the time this runs, so nothing here can
    /// change the reason the caller sees. What it *does* change is whether the
    /// Pod agrees with the ledger.
    ///
    /// When the compare-and-swap lands, the row durably owes a drop and the
    /// external drop reconciler owns it from there — `drop_required` is
    /// classified `DropOwed`, which is a state it acts on unconditionally. That
    /// path is unchanged.
    ///
    /// When the compare-and-swap is **refused**, this invocation has lost the
    /// row to another actor and has no ledger edge left to write. Before this
    /// slice that was the end of it: an `error!` line and a Pod still holding
    /// the full lifted limit, with the row back at `birth_confirmed` where
    /// `strandedness()` classifies a live owner as `Live` and skips it forever.
    /// See this module's header for the production measurement. So the refusal
    /// is now actuated instead of narrated.
    ///
    /// `surface` is `None` only on the one path that failed *because* no
    /// apiserver surface could be resolved — there is nothing to PATCH through
    /// and, having never reached the wire, nothing to undo.
    async fn require_drop(
        &self,
        surface: Option<&Arc<dyn LauncherResizeSurface>>,
        intent: &PodResizeIntent,
        from: BuildPodPermitState,
    ) {
        let Err(failure) = self
            .transition(intent, from, BuildPodPermitState::DropRequired)
            .await
        else {
            return;
        };
        tracing::error!(
            task_run_id = %intent.task_run_id,
            permit_id = %intent.permit_id,
            detail = %failure.detail,
            "resize lift: could not mark the permit drop-required after a failed lift; \
             returning the launcher to its birth limit directly"
        );
        let Some(surface) = surface else {
            tracing::error!(
                task_run_id = %intent.task_run_id,
                permit_id = %intent.permit_id,
                "resize lift: no apiserver surface to return the launcher through; the \
                 lift never reached the wire, so there is nothing to undo"
            );
            return;
        };
        self.return_to_birth_limit(surface, intent).await;
    }

    /// What the durable row currently claims about this Pod's launcher limit.
    ///
    /// Re-read, never inferred from the state this invocation last saw: by the
    /// time this runs the row has demonstrably moved under us at least once.
    async fn ledger_claim(&self, intent: &PodResizeIntent) -> LedgerClaim {
        let row = match self.permits.active(&intent.task_run_id).await {
            Ok(Some(row)) => row,
            // No capacity-active permit at all. Nothing durable governs this
            // Pod any more, so no row is claiming the ceiling and nothing will
            // ever reconcile a limit left on it. The birth clamp is the honest
            // reading — and the UID fence at the observation is what keeps that
            // from becoming a PATCH addressed to somebody else's Pod.
            Ok(None) => return LedgerClaim::BirthClamped { state: None },
            Err(error) => {
                tracing::warn!(
                    task_run_id = %intent.task_run_id,
                    %error,
                    "resize lift: durable permit unreadable while undoing a lift; the lift \
                     is unrecorded either way, so the launcher is returned to birth"
                );
                return LedgerClaim::BirthClamped { state: None };
            }
        };
        // The row must still govern the object this intent was captured
        // against. A permit whose identity names another Pod UID is not a
        // ledger entry about our launcher at all.
        if row.resize_identity.as_ref().map(|id| id.pod_uid.as_str())
            != Some(intent.pod_uid.as_str())
        {
            return LedgerClaim::OwnedByAnotherDriver;
        }
        match row.state {
            // The ledger records a lift. The Pod holding the ceiling is exactly
            // what the row admits, so the invariant already holds and taking it
            // away would strand whoever earned it.
            BuildPodPermitState::Lifted => LedgerClaim::Lifted,
            // Another invocation is mid-lift on this row. It owns the outcome,
            // it will settle its own row, and a PATCH from here would clamp a
            // lift that is still being driven.
            BuildPodPermitState::LiftApplying
                if row.resize_invocation_id.as_deref() != Some(intent.invocation_id.as_str()) =>
            {
                LedgerClaim::OwnedByAnotherDriver
            }
            state => LedgerClaim::BirthClamped { state: Some(state) },
        }
    }

    /// Undo a lift the ledger refused to record: PATCH the launcher back to
    /// [`BIRTH_CPU_MILLICORES`] and confirm it through the init-container
    /// status.
    ///
    /// # Safe under the concurrent reconciler
    ///
    /// Three properties, each of which the tests name a mutation for:
    ///
    /// 1. **Re-read before acting.** [`Self::ledger_claim`] decides from the
    ///    row as it is *now*, so a row another driver owns, or one that durably
    ///    records a lift, is left alone.
    /// 2. **Already-at-birth is success, not failure.** The first observation
    ///    reads the launcher's persisted `limits.cpu`; if it is already at or
    ///    below the birth clamp, somebody else got there first and this path
    ///    returns having issued **zero** PATCHes.
    /// 3. **The UID fence on every pass.** The observation is re-fenced before
    ///    each attempt, so a Pod deleted and recreated under the same name is
    ///    never the object that gets clamped.
    ///
    /// Deliberately NOT fenced on the launcher's `containerID` or its declared
    /// protocol, unlike a lift. Both of those exist to stop a *grant* being
    /// reasoned about a cgroup that is gone; a restarted launcher comes back
    /// holding whatever `spec.initContainers[..].limits.cpu` the failed lift
    /// left behind, so returning that spec to birth is still exactly right and
    /// refusing to would leave the strand in place.
    async fn return_to_birth_limit(
        &self,
        surface: &Arc<dyn LauncherResizeSurface>,
        intent: &PodResizeIntent,
    ) {
        let state = match self.ledger_claim(intent).await {
            LedgerClaim::Lifted => {
                tracing::info!(
                    task_run_id = %intent.task_run_id,
                    pod_uid = %intent.pod_uid,
                    "resize lift: the permit durably records a lift for this Pod; the \
                     launcher's limit is admitted and is not returned"
                );
                return;
            }
            LedgerClaim::OwnedByAnotherDriver => {
                tracing::info!(
                    task_run_id = %intent.task_run_id,
                    pod_uid = %intent.pod_uid,
                    "resize lift: another driver owns this permit's resize lifecycle; not \
                     patching"
                );
                return;
            }
            LedgerClaim::BirthClamped { state } => state,
        };

        let deadline = tokio::time::Instant::now() + self.budget;
        let mut patched = false;
        loop {
            let observed = match surface.observe_launcher(&intent.task_run_id).await {
                Ok(Some(observed)) => observed,
                // No Pod carries the label. Nothing holds the lifted limit.
                Ok(None) => {
                    tracing::info!(
                        task_run_id = %intent.task_run_id,
                        pod_uid = %intent.pod_uid,
                        "resize lift: no Pod carries the label; the lifted limit is moot"
                    );
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        task_run_id = %intent.task_run_id,
                        %error,
                        "resize lift: apiserver did not answer while returning the \
                         launcher to its birth limit"
                    );
                    if !self.wait_for_retry(deadline).await {
                        return self
                            .strand_alarm(intent, state, "the apiserver never answered")
                            .await;
                    }
                    continue;
                }
            };
            // THE UID FENCE. A Pod recreated under the same name belongs to
            // whoever created it; our lift is not on it and must not be undone
            // there.
            if observed.pod_uid != intent.pod_uid {
                tracing::info!(
                    task_run_id = %intent.task_run_id,
                    permit_pod_uid = %intent.pod_uid,
                    live_pod_uid = %observed.pod_uid,
                    "resize lift: the Pod was recreated under the same name; the lifted \
                     limit went with the object that held it"
                );
                return;
            }
            // ALREADY BACK AT BIRTH. Somebody else — the drop reconciler, most
            // likely the very actor that took this row away — got there first.
            // That is success, and it must cost zero PATCHes: re-PATCHing a
            // launcher that is already clamped is how an idempotent fallback
            // turns into a second writer fighting the first.
            if !patched
                && observed
                    .admitted_cpu_millicores
                    .is_some_and(|millicores| millicores <= BIRTH_CPU_MILLICORES)
            {
                tracing::info!(
                    task_run_id = %intent.task_run_id,
                    pod_uid = %intent.pod_uid,
                    "resize lift: the launcher is already at its birth limit; nothing to \
                     return"
                );
                return;
            }

            patched = true;
            match surface
                .resize_launcher_cpu(&observed.pod_name, BIRTH_CPU_MILLICORES)
                .await
            {
                // `resize_launcher_cpu` confirms through
                // `status.initContainerStatuses`, so `Ok` is the kubelet's
                // answer and not the apiserver's acknowledgement.
                Ok(()) => {
                    tracing::warn!(
                        task_run_id = %intent.task_run_id,
                        pod_uid = %intent.pod_uid,
                        birth_millicores = BIRTH_CPU_MILLICORES,
                        "resize lift: the ledger refused to record this lift, so the \
                         launcher was returned to its birth limit"
                    );
                    return;
                }
                Err(error) => {
                    if !is_retryable(&error) {
                        return self.strand_alarm(intent, state, &error.to_string()).await;
                    }
                    if !self.wait_for_retry(deadline).await {
                        return self.strand_alarm(intent, state, &error.to_string()).await;
                    }
                }
            }
        }
    }

    /// Sleep one poll interval if the budget allows it. `false` means the
    /// budget is gone and the caller must settle.
    async fn wait_for_retry(&self, deadline: tokio::time::Instant) -> bool {
        if tokio::time::Instant::now() + self.poll_interval >= deadline {
            return false;
        }
        tokio::time::sleep(self.poll_interval).await;
        true
    }

    /// The launcher could not be returned. Say so loudly, and — if the row is
    /// still one this invocation can move — leave a `drop_required` behind so
    /// the external reconciler has a state it will actually act on.
    ///
    /// `state` is the row's re-read state at the moment the fallback started,
    /// and `None` means there was no readable row to move. This is best-effort
    /// on top of best-effort: an unreachable apiserver is exactly the condition
    /// under which the durable hand-off is the only remaining lever.
    async fn strand_alarm(
        &self,
        intent: &PodResizeIntent,
        state: Option<BuildPodPermitState>,
        detail: &str,
    ) {
        tracing::error!(
            task_run_id = %intent.task_run_id,
            permit_id = %intent.permit_id,
            pod_uid = %intent.pod_uid,
            admitted_cpu_millicores = intent.admitted_cpu_millicores,
            detail,
            "resize lift: THE LAUNCHER IS STRANDED ABOVE ITS LEDGER — the lift could \
             neither be recorded nor undone"
        );
        let Some(state) = state else { return };
        if let Ok(TransitionBuildPodResizeLifecycleResult::Transitioned(_)) = self
            .permits
            .transition_resize_lifecycle(
                &intent.task_run_id,
                &intent.permit_id,
                intent.fencing_token,
                &intent.pod_uid,
                Some(&intent.invocation_id),
                state,
                BuildPodPermitState::DropRequired,
            )
            .await
        {
            tracing::warn!(
                task_run_id = %intent.task_run_id,
                "resize lift: handed the stranded launcher to the drop reconciler"
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
        let target = clamp_to_admitted_ceiling(intent)?;

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
                self.require_drop(None, intent, entered).await;
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
                    self.require_drop(Some(&surface), intent, BuildPodPermitState::LiftApplying)
                        .await;
                    return Err(failure);
                }
                Ok(())
            }
            Err(failure) => {
                self.require_drop(Some(&surface), intent, entered).await;
                Err(failure)
            }
        }
    }
}

/// THE LAST FENCE BEFORE THE WIRE: the millicores one PATCH body may carry.
///
/// `min(intent.target_millicores, intent.admitted_cpu_millicores)`, refusing
/// anything that is not a positive CPU quantity. Every PATCH this module issues
/// goes through it, so "zero PATCH bodies above the admitted ceiling" is a
/// property of the code that sends them rather than of the code that derived
/// the target.
///
/// # Why the clamp lives here and not only in `resize_authorization`
///
/// `0ppk-1a` clamped while *deriving* the target, as
/// `min(configured_leased_millicores, admitted)`. `gvix` deleted that first
/// term — a deployment-wide default has no business bounding a per-Pod lift, and
/// it held every per-project-override Pod below its own rendered lease. But
/// deleting it must not delete the safety property with it, and a clamp that
/// sits in the derivation only ever constrains the one caller that derives.
/// [`PodResizeIntent`] is a `pub` struct of `pub` fields; anything that builds
/// one — a future reconciler, a test, a second authority — reaches
/// `resize_launcher_cpu` through here. So the bound is enforced against the
/// value that is about to be sent, next to the call that sends it.
///
/// NAMED FAILING MUTATION for `0ppk-1a`'s acceptance criterion 2: replace the
/// `.min(ceiling)` below with `intent.target_millicores` and
/// `an_intent_above_its_own_ceiling_still_patches_at_the_ceiling` observes a
/// PATCH body above the ceiling.
///
/// # Errors
///
/// [`ResizeApplyFailure`] with [`DegradedUnleasedReason::CeilingUnusable`] when
/// the ceiling or the clamped target is not a positive millicore count. A
/// non-positive quantity is refused rather than sent: `0m` is a rejected PATCH
/// at best and an unbounded one at worst, and either way it is not a lift.
pub fn clamp_to_admitted_ceiling(intent: &PodResizeIntent) -> Result<u64, ResizeApplyFailure> {
    let unusable =
        |detail: String| ResizeApplyFailure::new(DegradedUnleasedReason::CeilingUnusable, detail);
    if intent.admitted_cpu_millicores <= 0 {
        return Err(unusable(format!(
            "permit carries admitted ceiling {}m, which is not a CPU quantity",
            intent.admitted_cpu_millicores
        )));
    }
    let clamped = intent.target_millicores.min(intent.admitted_cpu_millicores);
    if clamped != intent.target_millicores {
        tracing::warn!(
            task_run_id = %intent.task_run_id,
            pod_uid = %intent.pod_uid,
            requested_millicores = intent.target_millicores,
            admitted_cpu_millicores = intent.admitted_cpu_millicores,
            "resize lift: target exceeded the admitted ceiling and was clamped \
             before the PATCH"
        );
    }
    u64::try_from(clamped)
        .ok()
        .filter(|millicores| *millicores > 0)
        .ok_or_else(|| unusable(format!("clamped target {clamped}m is not a CPU quantity")))
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
        // The kubelet has not (re)published the launcher's init-container
        // status. Reaching this during a LIFT means the status went away after
        // the birth capture read it — a Pod being replaced underneath us — so it
        // is classified with the other "no Pod to fence against" answers rather
        // than as an identity fault. It is not `LiftStatusStale`: nothing stale
        // was read, there was nothing to read.
        LauncherObservationError::StatusNotPopulated { .. } => {
            DegradedUnleasedReason::LiftPodAbsent
        }
        LauncherObservationError::Ambiguous(inner) => return apply_failure(inner),
        LauncherObservationError::Api(_) => DegradedUnleasedReason::ResizeSurfaceUnavailable,
    };
    ResizeApplyFailure::new(reason, error.to_string())
}
