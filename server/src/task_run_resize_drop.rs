//! The fail-safe drop: returning a lifted launcher to its birth limit, or
//! destroying the Pod that refused to come back down.
//!
//! `0ppk-1c` raises `spec.initContainers[cgroup-launcher].resources.limits.cpu`
//! from 250m to a clamped ceiling for the duration of one invocation's build.
//! This module is the other half of that trade: **every** way an invocation can
//! end converges here, and the CPU the cluster lent is either given back or the
//! borrower is destroyed.
//!
//! # The two things that may release the lease
//!
//! Exactly two facts end a drop, and neither of them is a row this module wrote:
//!
//! 1. **250m read back from `status.initContainerStatuses[cgroup-launcher]`**,
//!    compared in millicores through [`djinn_k8s::pod_resize::CpuLimit`]. Never
//!    `status.containerStatuses` — the launcher is a native sidecar, so no
//!    regular container by that name legitimately exists, and a fixture that
//!    plants one there is the sharpest form of the false-confirmation trap.
//!    Never a Quantity string either: the apiserver canonicalises `4000m` to
//!    `4`, and #2861 watched a string comparison report `never reported 2000m;
//!    last observed Some(2000)`.
//! 2. **The exact Pod UID confirmed absent.** A Pod that no longer exists holds
//!    no limit. Absence is proven by observation — a fresh label-scoped GET that
//!    returns no Pod, or returns a Pod whose `metadata.uid` is not ours — and
//!    **never** by a DELETE's own acknowledgement. A 200 from `DELETE` says the
//!    apiserver accepted the request, not that the object is gone.
//!
//! `state = 'birth_confirmed'` is a LABEL. Writing it proves nothing, which is
//! why [`ResizeDropOutcome::Confirmed`] is only reachable after the PATCH has
//! been confirmed through the client's own init-container status rule.
//!
//! # Two ledgers, and the one that must not move first
//!
//! `BuildLeaseService::release` writes `build_leases`. This lifecycle lives in
//! `build_pod_permits`. They are DIFFERENT STORES, and "the lease was released"
//! is a true statement about the wrong one unless it names which. The gate is
//! [`ResizeDropOutcome::releases_lease`]: while the permit row sits in
//! `drop_required`, `drop_applying` or `quarantined`, no outcome that permits a
//! release exists, so the build lease row stays occupying and terminal is not
//! acknowledged. See `direct_services.rs`'s `release_lease` handler, which is
//! where that gate is actually applied.
//!
//! # Quarantine is unbounded on purpose
//!
//! A Pod that could not be dropped is holding CPU nobody is paying for, and the
//! apiserver being unreachable is not a reason to stop trying to destroy it —
//! it is a reason the lease must stay held. [`Self::confirm_absence`] has no
//! deadline and no attempt cap. Bounding it would trade an unbounded retry for
//! an unbounded overcommit.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use djinn_agent::task_run_resize_drop_gate::{
    ResizeDropGateRequest, ResizeDropVerdict, TaskRunResizeDropGate,
};
use djinn_db::{
    BuildPodPermitRepository, BuildPodPermitState, BuildPodResizeIdentity,
    ReleaseBuildPodPermitResult, TransitionBuildPodResizeLifecycleResult,
};
use djinn_k8s::pod_resize::PodResizeError;
use djinn_k8s::runtime::{LauncherObservationError, ObservedLauncherSidecar};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_supervisor::services::DegradedUnleasedReason;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::task_run_resize_bootstrap::{BIRTH_CPU_MILLICORES, TaskRunPodSurface};

/// First backoff between confirmation attempts.
pub const DROP_INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Ceiling on the backoff. The kubelet actuates in milliseconds when it is going
/// to actuate at all; a longer gap only spends the confirmation window without
/// buying more information.
pub const DROP_MAX_BACKOFF: Duration = Duration::from_secs(2);

/// How long the drop waits for `status.initContainerStatuses` to report 250m
/// before quarantining the Pod.
pub const DROP_CONFIRMATION_BUDGET: Duration = Duration::from_secs(30);

/// The durable release reason recorded when a quarantined Pod is confirmed gone.
/// `build_pod_permits_release_check` caps this at 64 characters.
const QUARANTINE_RELEASE_REASON: &str = "resize_quarantine_pod_deleted";

// ── The injected clock ─────────────────────────────────────────────────────

/// The passage of time, as this module consumes it.
///
/// Behind a trait because acceptance criterion 5 is a statement about the
/// *observed delay sequence*, and a test that measured wall-clock sleeps would
/// take 30 seconds to make it. A fake that records what it was asked to wait for
/// is what turns "bounded exponential backoff from 250ms to 2s" into an
/// assertion that fails when the cap is removed.
#[async_trait]
pub trait ResizeDropClock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;

    /// Wait for `duration`.
    async fn sleep(&self, duration: Duration);
}

/// The production clock.
#[derive(Debug, Default)]
pub struct SystemResizeDropClock;

#[async_trait]
impl ResizeDropClock for SystemResizeDropClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

// ── Inputs ─────────────────────────────────────────────────────────────────

/// Which terminal path is asking for the drop.
///
/// Purely diagnostic to the mechanism — the drop is identical for all of them,
/// which is the property acceptance criterion 3 exists to hold onto. It is a
/// closed enum rather than a string so a new terminal path is a compile error at
/// the mapping sites instead of an unnamed cause in a log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeDropCause {
    /// The child exited zero.
    NormalExit,
    /// The child exited non-zero.
    NonZeroExit,
    /// The invocation deadline passed.
    TimedOut,
    /// The invocation was cancelled.
    Cancelled,
    /// Three consecutive `LeaseUnavailable` responses forced a cancel.
    LeaseUnavailableForcedCancel,
    /// The grant was denied after a lift had already been attempted.
    GrantDenied,
    /// `0ppk-1c` folded the grant into `DegradedUnleased`.
    DegradedUnleased,
    /// The worker's connection died; observed by `watch_infra_death`.
    WorkerDisconnect,
    /// The Pod terminated; observed by `watch_infra_death`.
    PodTerminated,
    /// A durable row was found mid-lift by a process that did not start it.
    ServerRestart,
}

impl ResizeDropCause {
    /// The stable spelling for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NormalExit => "normal_exit",
            Self::NonZeroExit => "non_zero_exit",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::LeaseUnavailableForcedCancel => "lease_unavailable_forced_cancel",
            Self::GrantDenied => "grant_denied",
            Self::DegradedUnleased => "degraded_unleased",
            Self::WorkerDisconnect => "worker_disconnect",
            Self::PodTerminated => "pod_terminated",
            Self::ServerRestart => "server_restart",
        }
    }
}

/// One request to return a task run's launcher to its birth limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeDropRequest {
    /// The task run whose permit row this is.
    pub task_run_id: String,
    /// The invocation this drop retires, when the caller has one.
    ///
    /// `None` is the infrastructure-observer shape: `watch_infra_death` sees a
    /// Pod die, not an invocation end, so it has no invocation to name and
    /// drives whichever one the row currently carries. A caller that DOES name
    /// one and does not match the row is stale — its lift never took, because a
    /// second invocation can only claim the row from `birth_confirmed`, which
    /// requires the first one's drop to have completed.
    pub invocation_id: Option<String>,
    /// Which terminal path is asking.
    pub cause: ResizeDropCause,
}

// ── Outcomes ───────────────────────────────────────────────────────────────

/// What a drop settled on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResizeDropOutcome {
    /// 250m was read back from the launcher's init-container status. The permit
    /// row is back at `birth_confirmed`.
    Confirmed {
        /// The Pod the confirmation was fenced to.
        pod_uid: String,
    },
    /// The exact Pod UID is confirmed absent, so nothing holds a raised limit.
    /// The permit row is left nonterminal for `0ppk-3`'s reconciler.
    PodUidAbsent {
        /// The Pod UID proven absent.
        pod_uid: String,
    },
    /// The permit carries no resize identity: a `leaf-v1` run, or one whose Pod
    /// was never captured. There is no container limit to return.
    NotResizeGoverned,
    /// No active permit row exists for this task run.
    NoActivePermit,
    /// Another invocation owns this row's resize lifecycle, so the caller's lift
    /// never took and it owes no drop.
    Fenced {
        /// The invocation that does own the row, if any.
        owner: Option<String>,
    },
    /// The drop could not be confirmed. The Pod was quarantined, UID-fenced
    /// deleted, its absence proven, and the permit released.
    QuarantinedPodDeleted {
        /// The Pod that was destroyed.
        pod_uid: String,
        /// Why the drop could not be confirmed.
        reason: DegradedUnleasedReason,
    },
    /// Durable storage or the apiserver could not answer, and the drop is not
    /// settled. **Deliberately does not release the lease.**
    Unavailable(String),
}

impl ResizeDropOutcome {
    /// Whether the durable CPU lease may now be released and terminal
    /// acknowledged.
    ///
    /// This is the gate acceptance criterion 6 names. Every `true` arm rests on
    /// a fact the cluster reported — 250m in init-container status, or an absent
    /// Pod UID, or a permit that never governed a resize at all. `Unavailable`
    /// is `false` because an unanswered apiserver is not evidence that the CPU
    /// came back.
    #[must_use]
    pub const fn releases_lease(&self) -> bool {
        match self {
            Self::Confirmed { .. }
            | Self::PodUidAbsent { .. }
            | Self::NotResizeGoverned
            | Self::NoActivePermit
            | Self::Fenced { .. }
            | Self::QuarantinedPodDeleted { .. } => true,
            Self::Unavailable(_) => false,
        }
    }
}

/// The resolved subject of a drop: everything the fences are compared against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeDropSubject {
    /// Task run whose permit row this is.
    pub task_run_id: String,
    /// Immutable permit identity.
    pub permit_id: String,
    /// Monotonic ownership fence.
    pub fencing_token: i64,
    /// The write-once Pod UID every PATCH, DELETE and absence proof is bound to.
    pub pod_uid: String,
    /// The Pod's name, which is all `pods/resize` accepts.
    pub pod_name: String,
    /// `status.initContainerStatuses[..].containerID` as captured.
    pub launcher_container_id: String,
    /// The protocol the server resolved for this Pod.
    pub effective_launcher_protocol: LauncherAuthorityProtocol,
    /// The invocation the row carries, and therefore the invocation half of the
    /// `(task_run_id, pod_uid, resize_invocation_id)` compare-and-swap.
    pub invocation_id: Option<String>,
}

impl ResizeDropSubject {
    fn fence(&self) -> Option<&str> {
        self.invocation_id.as_deref()
    }
}

/// What [`TaskRunResizeDrop::require_drop`] resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResizeDropRequirement {
    /// The permit row now owes a drop: it is at `drop_required`, or already at
    /// `drop_applying` from an earlier attempt this call resumes.
    Required(Box<ResizeDropSubject>),
    /// The row is already quarantined; only the absence loop remains.
    Quarantined(Box<ResizeDropSubject>),
    /// Nothing to do, for a reason that already settles the lease question.
    Settled(ResizeDropOutcome),
}

// ── The drop ───────────────────────────────────────────────────────────────

/// Composes the durable permit relation, the apiserver surface and a clock into
/// the fail-safe drop.
pub struct TaskRunResizeDrop {
    permits: Arc<BuildPodPermitRepository>,
    surface: Arc<dyn TaskRunPodSurface>,
    clock: Arc<dyn ResizeDropClock>,
    budget: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    confirmation_attempts: AtomicU64,
    quarantine_attempts: AtomicU64,
}

impl TaskRunResizeDrop {
    /// Wire a drop to a permit repository and an apiserver surface, on the
    /// production clock and the specified schedule.
    #[must_use]
    pub fn new(
        permits: Arc<BuildPodPermitRepository>,
        surface: Arc<dyn TaskRunPodSurface>,
    ) -> Self {
        Self::with_clock(permits, surface, Arc::new(SystemResizeDropClock))
    }

    /// Same composition against an injected clock.
    #[must_use]
    pub fn with_clock(
        permits: Arc<BuildPodPermitRepository>,
        surface: Arc<dyn TaskRunPodSurface>,
        clock: Arc<dyn ResizeDropClock>,
    ) -> Self {
        Self {
            permits,
            surface,
            clock,
            budget: DROP_CONFIRMATION_BUDGET,
            initial_backoff: DROP_INITIAL_BACKOFF,
            max_backoff: DROP_MAX_BACKOFF,
            confirmation_attempts: AtomicU64::new(0),
            quarantine_attempts: AtomicU64::new(0),
        }
    }

    /// How many confirmation attempts have been made across every drop this
    /// object has driven. Each one issues its own fresh GET.
    #[must_use]
    pub fn confirmation_attempts(&self) -> u64 {
        self.confirmation_attempts.load(Ordering::SeqCst)
    }

    /// How many absence-confirmation passes the quarantine loop has made.
    #[must_use]
    pub fn quarantine_attempts(&self) -> u64 {
        self.quarantine_attempts.load(Ordering::SeqCst)
    }

    /// Resolve the request and move the permit row to `drop_required`.
    ///
    /// Split from [`Self::drop_to_birth`] so that "every terminal path reaches
    /// `drop_required`" is a state a test can observe. Driving the whole
    /// sequence would walk straight through `drop_required` to
    /// `birth_confirmed`, and a table that could only read the end state could
    /// not tell a path that was hooked up from one that never was.
    pub async fn require_drop(&self, request: &ResizeDropRequest) -> ResizeDropRequirement {
        let row = match self.permits.active(&request.task_run_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return ResizeDropRequirement::Settled(ResizeDropOutcome::NoActivePermit),
            Err(error) => {
                return ResizeDropRequirement::Settled(ResizeDropOutcome::Unavailable(format!(
                    "durable permit unreadable: {error}"
                )));
            }
        };
        let Some(identity) = row.resize_identity.clone() else {
            return ResizeDropRequirement::Settled(ResizeDropOutcome::NotResizeGoverned);
        };

        // THE INVOCATION FENCE, at the entry to the drop. A caller that names an
        // invocation the row does not carry never lifted this Pod.
        if let Some(claimed) = request.invocation_id.as_deref()
            && row.resize_invocation_id.as_deref() != Some(claimed)
        {
            return ResizeDropRequirement::Settled(ResizeDropOutcome::Fenced {
                owner: row.resize_invocation_id.clone(),
            });
        }

        let Ok(protocol) = identity
            .effective_launcher_protocol
            .parse::<LauncherAuthorityProtocol>()
        else {
            return ResizeDropRequirement::Settled(ResizeDropOutcome::NotResizeGoverned);
        };
        let subject = Box::new(subject_of(&row.task_run_id, &row, &identity, protocol));

        match row.state {
            // `birth_confirmed` is reachable here: an infrastructure observer
            // can see a Pod die before any invocation ever lifted it. The edge
            // is legal and the drop that follows is a no-op confirmation.
            state @ (BuildPodPermitState::BirthConfirmed
            | BuildPodPermitState::LiftApplying
            | BuildPodPermitState::Lifted) => {
                match self
                    .transition(&subject, state, BuildPodPermitState::DropRequired)
                    .await
                {
                    Ok(()) => ResizeDropRequirement::Required(subject),
                    Err(detail) => {
                        ResizeDropRequirement::Settled(ResizeDropOutcome::Unavailable(detail))
                    }
                }
            }
            // Resume: an earlier attempt already got this far.
            BuildPodPermitState::DropRequired | BuildPodPermitState::DropApplying => {
                ResizeDropRequirement::Required(subject)
            }
            BuildPodPermitState::Quarantined => ResizeDropRequirement::Quarantined(subject),
            BuildPodPermitState::Acquired | BuildPodPermitState::JobCreated => {
                ResizeDropRequirement::Settled(ResizeDropOutcome::NotResizeGoverned)
            }
            BuildPodPermitState::Released => {
                ResizeDropRequirement::Settled(ResizeDropOutcome::NoActivePermit)
            }
        }
    }

    /// Drive one terminal path all the way to a settled answer.
    ///
    /// Returns only when 250m is confirmed, the Pod UID is proven absent, the
    /// quarantined Pod is proven destroyed, or the request had nothing to do.
    /// The quarantine branch has no deadline: cancelling this future is the only
    /// way out of it, and doing so leaves the row durably `quarantined` and the
    /// lease durably held, which is the safe direction.
    pub async fn drop_to_birth(&self, request: &ResizeDropRequest) -> ResizeDropOutcome {
        match self.require_drop(request).await {
            ResizeDropRequirement::Settled(outcome) => outcome,
            ResizeDropRequirement::Quarantined(subject) => {
                self.confirm_absence(&subject, DegradedUnleasedReason::LiftDeadlineExceeded)
                    .await
            }
            ResizeDropRequirement::Required(subject) => {
                info!(
                    task_run_id = %subject.task_run_id,
                    pod_uid = %subject.pod_uid,
                    cause = request.cause.as_str(),
                    invocation_id = ?subject.invocation_id,
                    "task_run_resize_drop: returning the launcher to its birth limit"
                );
                self.apply_drop(&subject).await
            }
        }
    }

    /// `drop_required → drop_applying`, then PATCH-and-confirm on the schedule.
    async fn apply_drop(&self, subject: &ResizeDropSubject) -> ResizeDropOutcome {
        if let Err(detail) = self
            .transition(
                subject,
                BuildPodPermitState::DropRequired,
                BuildPodPermitState::DropApplying,
            )
            .await
        {
            // A self-transition from `drop_applying` is legal and idempotent, so
            // the only way here is a genuinely lost lifecycle.
            if !self.is_drop_applying(subject).await {
                return ResizeDropOutcome::Unavailable(detail);
            }
        }

        let deadline = self.clock.now() + self.budget;
        let mut backoff = self.initial_backoff;

        loop {
            self.confirmation_attempts.fetch_add(1, Ordering::SeqCst);

            // A FRESH GET, with the UID, protocol and launcher-container-id
            // fences, before EVERY attempt. Caching the Pod between attempts
            // would let a PATCH land on an object that was replaced mid-drop.
            // The most specific thing THIS attempt saw. Every arm either
            // returns a settled outcome or yields a reason that waiting could
            // still change, so the deadline below always reports a fact somebody
            // actually observed rather than a default nobody measured.
            let unconfirmed = match self.observe_and_fence(subject).await {
                FenceOutcome::Fenced(observed) => {
                    match self
                        .surface
                        .resize_launcher_cpu(&observed.pod_name, BIRTH_CPU_MILLICORES)
                        .await
                    {
                        Ok(()) => {
                            // The confirming fence. The client's own final GET
                            // proves the LIMIT through
                            // `status.initContainerStatuses`; only re-reading
                            // the UID and container id proves that limit belongs
                            // to the object this drop was authorized against.
                            match self.observe_and_fence(subject).await {
                                FenceOutcome::Fenced(_) => {
                                    return self.settle_confirmed(subject).await;
                                }
                                FenceOutcome::PodUidAbsent => {
                                    return ResizeDropOutcome::PodUidAbsent {
                                        pod_uid: subject.pod_uid.clone(),
                                    };
                                }
                                FenceOutcome::Quarantine(reason) => {
                                    return self.quarantine(subject, reason).await;
                                }
                                FenceOutcome::Retry(reason) => reason,
                            }
                        }
                        Err(error) => {
                            let (reason, retryable) = classify(&error);
                            if !retryable {
                                return self.quarantine(subject, reason).await;
                            }
                            reason
                        }
                    }
                }
                FenceOutcome::PodUidAbsent => {
                    // The borrower is gone. Nothing holds the raised limit, and
                    // there is no object left to PATCH or to destroy.
                    info!(
                        task_run_id = %subject.task_run_id,
                        pod_uid = %subject.pod_uid,
                        "task_run_resize_drop: Pod UID confirmed absent; the drop is moot"
                    );
                    return ResizeDropOutcome::PodUidAbsent {
                        pod_uid: subject.pod_uid.clone(),
                    };
                }
                FenceOutcome::Quarantine(reason) => return self.quarantine(subject, reason).await,
                FenceOutcome::Retry(reason) => reason,
            };

            // The deadline is checked against the sleep the loop is ABOUT to
            // take, not against the time already spent, so the window is never
            // overrun by up to one full backoff.
            if self.clock.now() + backoff >= deadline {
                return self.quarantine(subject, unconfirmed).await;
            }
            self.clock.sleep(backoff).await;
            backoff = (backoff * 2).min(self.max_backoff);
        }
    }

    /// Fresh label-scoped GET plus the three identity fences.
    async fn observe_and_fence(&self, subject: &ResizeDropSubject) -> FenceOutcome {
        let observed = match self.surface.observe_launcher(&subject.task_run_id).await {
            Ok(Some(observed)) => observed,
            // No Pod carries the label, so ours is certainly not among them.
            Ok(None) => return FenceOutcome::PodUidAbsent,
            Err(LauncherObservationError::Api(error)) => {
                warn!(
                    task_run_id = %subject.task_run_id,
                    %error,
                    "task_run_resize_drop: apiserver did not answer the confirmation GET"
                );
                return FenceOutcome::Retry(DegradedUnleasedReason::ResizeSurfaceUnavailable);
            }
            Err(LauncherObservationError::Incomplete { field }) => {
                warn!(
                    task_run_id = %subject.task_run_id,
                    field,
                    "task_run_resize_drop: stored Pod is not fully admitted; retrying"
                );
                return FenceOutcome::Retry(DegradedUnleasedReason::LiftPodAbsent);
            }
            Err(LauncherObservationError::Ambiguous(_)) => {
                return FenceOutcome::Quarantine(DegradedUnleasedReason::LiftIdentityAmbiguous);
            }
        };

        // THE UID FENCE. A Pod deleted and recreated under the same name is a
        // different object; ours is gone, and the replacement belongs to
        // whoever created it. Never PATCH it and never DELETE it.
        if observed.pod_uid != subject.pod_uid {
            return FenceOutcome::PodUidAbsent;
        }

        // THE PROTOCOL FENCE. Exactly one authority governs an admitted Pod.
        let declared = observed
            .observed_protocol
            .as_deref()
            .and_then(|raw| raw.parse::<LauncherAuthorityProtocol>().ok());
        if declared != Some(subject.effective_launcher_protocol) {
            return FenceOutcome::Quarantine(DegradedUnleasedReason::LauncherProtocolChanged);
        }

        // THE RESTART FENCE. A restarted launcher is a different cgroup than the
        // one the lift was reasoned about, and the Pod still carries whatever
        // container limit the lift left behind.
        if observed.launcher_container_id.as_deref() != Some(subject.launcher_container_id.as_str())
        {
            return FenceOutcome::Quarantine(DegradedUnleasedReason::LauncherRestarted);
        }

        FenceOutcome::Fenced(observed)
    }

    /// `drop_applying → birth_confirmed`, only ever after a confirmed 250m.
    async fn settle_confirmed(&self, subject: &ResizeDropSubject) -> ResizeDropOutcome {
        match self
            .transition(
                subject,
                BuildPodPermitState::DropApplying,
                BuildPodPermitState::BirthConfirmed,
            )
            .await
        {
            Ok(()) => {
                info!(
                    task_run_id = %subject.task_run_id,
                    pod_uid = %subject.pod_uid,
                    birth_millicores = BIRTH_CPU_MILLICORES,
                    "task_run_resize_drop: birth limit confirmed from init-container status"
                );
                ResizeDropOutcome::Confirmed {
                    pod_uid: subject.pod_uid.clone(),
                }
            }
            // The Pod came back down but the ledger did not move. Holding the
            // lease is the only answer that keeps the two in agreement.
            Err(detail) => ResizeDropOutcome::Unavailable(detail),
        }
    }

    /// Fail safe: mark the row quarantined, then destroy the Pod and prove it.
    async fn quarantine(
        &self,
        subject: &ResizeDropSubject,
        reason: DegradedUnleasedReason,
    ) -> ResizeDropOutcome {
        warn!(
            task_run_id = %subject.task_run_id,
            pod_uid = %subject.pod_uid,
            ?reason,
            "task_run_resize_drop: the launcher would not return to its birth limit; quarantining"
        );
        for from in [
            BuildPodPermitState::DropApplying,
            BuildPodPermitState::DropRequired,
            BuildPodPermitState::Lifted,
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::BirthConfirmed,
        ] {
            if self
                .transition(subject, from, BuildPodPermitState::Quarantined)
                .await
                .is_ok()
            {
                break;
            }
        }
        self.confirm_absence(subject, reason).await
    }

    /// Destroy the quarantined Pod and prove it gone. **Unbounded.**
    ///
    /// Absence is established by observation, never by the DELETE's own answer:
    /// a `200` says the apiserver accepted the request. Bounding this loop is
    /// acceptance criterion 7's named mutation — with a bound, an apiserver
    /// outage would end with the lease released and a Pod still holding a lifted
    /// ceiling.
    async fn confirm_absence(
        &self,
        subject: &ResizeDropSubject,
        reason: DegradedUnleasedReason,
    ) -> ResizeDropOutcome {
        let mut backoff = self.initial_backoff;
        loop {
            self.quarantine_attempts.fetch_add(1, Ordering::SeqCst);
            match self.surface.observe_launcher(&subject.task_run_id).await {
                // Proven absent: no Pod at all, or a Pod that is not ours.
                Ok(None) => return self.settle_quarantine(subject, reason).await,
                Ok(Some(observed)) if observed.pod_uid != subject.pod_uid => {
                    return self.settle_quarantine(subject, reason).await;
                }
                Ok(Some(_)) => {
                    // Still ours, still there. THE UID PRECONDITION on this
                    // delete is what stops a Pod recreated under the same name
                    // from being destroyed in its place — the arm above is why
                    // that case never reaches here at all.
                    if let Err(error) = self
                        .surface
                        .uid_fenced_delete(&subject.task_run_id, &subject.pod_uid)
                        .await
                    {
                        warn!(
                            task_run_id = %subject.task_run_id,
                            pod_uid = %subject.pod_uid,
                            %error,
                            "task_run_resize_drop: UID-fenced delete failed; retrying"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        task_run_id = %subject.task_run_id,
                        pod_uid = %subject.pod_uid,
                        %error,
                        "task_run_resize_drop: cannot confirm Pod absence; holding the lease"
                    );
                }
            }
            self.clock.sleep(backoff).await;
            backoff = (backoff * 2).min(self.max_backoff);
        }
    }

    /// `quarantined → released`, the only edge out of quarantine.
    async fn settle_quarantine(
        &self,
        subject: &ResizeDropSubject,
        reason: DegradedUnleasedReason,
    ) -> ResizeDropOutcome {
        match self
            .permits
            .release(
                &subject.task_run_id,
                &subject.permit_id,
                subject.fencing_token,
                QUARANTINE_RELEASE_REASON,
            )
            .await
        {
            Ok(
                ReleaseBuildPodPermitResult::Released(_)
                | ReleaseBuildPodPermitResult::AlreadyReleased(_),
            ) => {
                warn!(
                    task_run_id = %subject.task_run_id,
                    pod_uid = %subject.pod_uid,
                    ?reason,
                    "task_run_resize_drop: quarantined Pod is confirmed absent; permit released. \
                     This task run can never capture a replacement Pod — recovery is a NEW task run."
                );
                ResizeDropOutcome::QuarantinedPodDeleted {
                    pod_uid: subject.pod_uid.clone(),
                    reason,
                }
            }
            Ok(ReleaseBuildPodPermitResult::Rejected) => ResizeDropOutcome::Unavailable(format!(
                "permit {} refused the quarantine release",
                subject.permit_id
            )),
            Err(error) => {
                ResizeDropOutcome::Unavailable(format!("durable permit unreleasable: {error}"))
            }
        }
    }

    /// One lifecycle compare-and-swap, fenced on the permit identity, the Pod
    /// UID and the owning invocation.
    async fn transition(
        &self,
        subject: &ResizeDropSubject,
        expected: BuildPodPermitState,
        next: BuildPodPermitState,
    ) -> Result<(), String> {
        match self
            .permits
            .transition_resize_lifecycle(
                &subject.task_run_id,
                &subject.permit_id,
                subject.fencing_token,
                &subject.pod_uid,
                subject.fence(),
                expected,
                next,
            )
            .await
        {
            Ok(TransitionBuildPodResizeLifecycleResult::Transitioned(_)) => Ok(()),
            Ok(TransitionBuildPodResizeLifecycleResult::Rejected) => Err(format!(
                "permit {} refused the {expected:?} -> {next:?} transition",
                subject.permit_id
            )),
            Err(error) => {
                error!(
                    task_run_id = %subject.task_run_id,
                    %error,
                    "task_run_resize_drop: durable permit lifecycle unwritable"
                );
                Err(format!("durable permit lifecycle unwritable: {error}"))
            }
        }
    }

    async fn is_drop_applying(&self, subject: &ResizeDropSubject) -> bool {
        matches!(
            self.permits.active(&subject.task_run_id).await,
            Ok(Some(row)) if row.state == BuildPodPermitState::DropApplying
        )
    }
}

// ── The production composition ─────────────────────────────────────────────

/// How long the `release_lease` handler waits for a drop before answering.
///
/// This bounds the RPC, **not** the drop. A handler that gives up returns
/// [`ResizeDropVerdict::Held`], the durable row keeps whatever nonterminal
/// resize state it reached, and `build_leases` does not move — so the safety
/// property survives the timeout and `0ppk-3`'s reconciler picks the row up.
/// Bounding the quarantine loop itself instead would end an apiserver outage
/// with the lease released and a Pod still holding a lifted ceiling.
pub const DROP_GATE_BUDGET: Duration = Duration::from_secs(45);

/// Where the bridge gets its apiserver surface.
enum SurfaceSource {
    /// Built once, lazily, from ambient cluster configuration — the composition
    /// root is synchronous and building a `kube::Client` is not, and a server
    /// with no kubeconfig must still boot. A server that never resolves one
    /// holds every lease it is asked to release, which is the fail-closed
    /// answer: an unreachable apiserver is not evidence that CPU came back.
    Ambient(tokio::sync::OnceCell<Arc<dyn TaskRunPodSurface>>),
    /// Supplied by the caller. Fixtures use this.
    Fixed(Arc<dyn TaskRunPodSurface>),
}

/// The server's fail-safe drop, as the terminal seam consumes it.
pub struct TaskRunResizeDropBridge {
    db: djinn_db::Database,
    surface: SurfaceSource,
    clock: Arc<dyn ResizeDropClock>,
    budget: Duration,
}

impl TaskRunResizeDropBridge {
    /// The production constructor: durable permits over `db`, apiserver surface
    /// resolved from ambient cluster configuration on first terminal.
    #[must_use]
    pub fn from_env(db: djinn_db::Database) -> Self {
        Self {
            db,
            surface: SurfaceSource::Ambient(tokio::sync::OnceCell::new()),
            clock: Arc::new(SystemResizeDropClock),
            budget: DROP_GATE_BUDGET,
        }
    }

    /// Same composition against a caller-supplied surface and clock.
    #[must_use]
    pub fn with_surface(
        db: djinn_db::Database,
        surface: Arc<dyn TaskRunPodSurface>,
        clock: Arc<dyn ResizeDropClock>,
    ) -> Self {
        Self {
            db,
            surface: SurfaceSource::Fixed(surface),
            clock,
            budget: DROP_GATE_BUDGET,
        }
    }

    /// Override the handler's wait budget.
    #[must_use]
    pub const fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    async fn surface(&self) -> Result<Arc<dyn TaskRunPodSurface>, String> {
        match &self.surface {
            SurfaceSource::Fixed(surface) => Ok(Arc::clone(surface)),
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

    /// The same apiserver surface the drop itself patches through.
    ///
    /// Exposed for `0ppk-3`'s reconciler, whose strand predicate has to ask
    /// "is the exact Pod UID still there?" before it decides a live build's
    /// launcher is the reconciler's responsibility. Handing out the composed
    /// surface rather than letting the caller build its own is what keeps the
    /// reconciler's read and the drop's PATCH pointed at the same cluster —
    /// and, in a fixture, at the same stored Pod.
    ///
    /// # Errors
    ///
    /// The rendered failure from resolving an ambient `kube::Client`.
    pub async fn resize_pod_surface(&self) -> Result<Arc<dyn TaskRunPodSurface>, String> {
        self.surface().await
    }

    /// Build the fail-safe drop this bridge composes.
    ///
    /// **The one constructor.** `0ppk-3`'s reconciler resumes stranded rows
    /// through this, so the backoff schedule, the 30-second confirmation
    /// budget, the fresh-GET fences and the UID-fenced deletion exist exactly
    /// once. A reconciler that built its own `TaskRunResizeDrop` from its own
    /// constants would be a second copy of the retry schedule and a second
    /// thing to drift.
    ///
    /// # Errors
    ///
    /// The rendered failure from resolving an ambient `kube::Client`.
    pub async fn resize_drop_mechanism(&self) -> Result<TaskRunResizeDrop, String> {
        Ok(TaskRunResizeDrop::with_clock(
            Arc::new(BuildPodPermitRepository::new(self.db.clone())),
            self.surface().await?,
            Arc::clone(&self.clock),
        ))
    }
}

#[async_trait]
impl TaskRunResizeDropGate for TaskRunResizeDropBridge {
    async fn confirm_drop(&self, request: &ResizeDropGateRequest) -> ResizeDropVerdict {
        // The SAME constructor `0ppk-3`'s reconciler uses. One schedule, one
        // budget, one set of fences, reached from both the living worker's
        // terminal path and the reconciler that speaks for a dead one.
        let mechanism = match self.resize_drop_mechanism().await {
            Ok(mechanism) => mechanism,
            Err(error) => {
                return ResizeDropVerdict::Held {
                    detail: format!("no apiserver surface for the resize drop: {error}"),
                };
            }
        };
        let drop_request = ResizeDropRequest {
            task_run_id: request.task_run_id.clone(),
            invocation_id: Some(request.invocation_id.clone()),
            cause: ResizeDropCause::NormalExit,
        };
        match tokio::time::timeout(self.budget, mechanism.drop_to_birth(&drop_request)).await {
            Ok(outcome) if outcome.releases_lease() => ResizeDropVerdict::Releasable,
            Ok(outcome) => ResizeDropVerdict::Held {
                detail: format!("{outcome:?}"),
            },
            Err(_) => ResizeDropVerdict::Held {
                detail: format!(
                    "the drop for task run {} did not settle within {:?}; the permit row \
                     retains its resize state and the lease stays held",
                    request.task_run_id, self.budget
                ),
            },
        }
    }
}

/// What one fenced observation decided.
enum FenceOutcome {
    /// The live Pod is ours, on the expected protocol, with the launcher we
    /// captured.
    Fenced(ObservedLauncherSidecar),
    /// Our exact Pod UID is not present.
    PodUidAbsent,
    /// The Pod is present but ungovernable. Fail safe.
    Quarantine(DegradedUnleasedReason),
    /// Waiting could still change this answer.
    Retry(DegradedUnleasedReason),
}

/// Map one `pods/resize` failure onto its settled reason and whether waiting
/// could change it.
///
/// Deliberately the same rule `djinn_coordinator::resize_lift` applies to a
/// lift: only an unconfirmed *status* is retryable, because the kubelet actuates
/// asynchronously. An apiserver verdict (403/422), a transport failure or an
/// unparseable quantity is settled, and re-asking spends the window for nothing.
fn classify(error: &PodResizeError) -> (DegradedUnleasedReason, bool) {
    use djinn_k8s::pod_resize::NotConfirmed;
    match error {
        PodResizeError::NotConfirmed(NotConfirmed::ResizePending) => {
            (DegradedUnleasedReason::LiftResizePending, true)
        }
        PodResizeError::NotConfirmed(NotConfirmed::StatusLimitAbsent) => {
            (DegradedUnleasedReason::LiftStatusAbsent, true)
        }
        PodResizeError::NotConfirmed(NotConfirmed::StatusStale { .. }) => {
            (DegradedUnleasedReason::LiftStatusStale, true)
        }
        PodResizeError::NotConfirmed(NotConfirmed::StatusLimitUnparseable { .. }) => {
            (DegradedUnleasedReason::LiftStatusStale, false)
        }
        PodResizeError::LauncherIdentityAmbiguous { .. } => {
            (DegradedUnleasedReason::LiftIdentityAmbiguous, false)
        }
        PodResizeError::InvalidCpuQuantity { .. } => {
            (DegradedUnleasedReason::CeilingUnusable, false)
        }
        // Classified on the NUMERIC status, never on the rendered message.
        PodResizeError::Api { status, .. } => (
            match status {
                Some(403) => DegradedUnleasedReason::ResizeForbidden,
                Some(422) => DegradedUnleasedReason::ResizeRejected,
                _ => DegradedUnleasedReason::ResizeSurfaceUnavailable,
            },
            false,
        ),
    }
}

fn subject_of(
    task_run_id: &str,
    row: &djinn_db::BuildPodPermitRow,
    identity: &BuildPodResizeIdentity,
    protocol: LauncherAuthorityProtocol,
) -> ResizeDropSubject {
    ResizeDropSubject {
        task_run_id: task_run_id.to_owned(),
        permit_id: row.permit_id.clone(),
        fencing_token: row.fencing_token,
        pod_uid: identity.pod_uid.clone(),
        pod_name: identity.pod_name.clone(),
        launcher_container_id: identity.launcher_container_id.clone(),
        effective_launcher_protocol: protocol,
        invocation_id: row.resize_invocation_id.clone(),
    }
}

#[cfg(test)]
#[path = "task_run_resize_drop_tests.rs"]
mod tests;
