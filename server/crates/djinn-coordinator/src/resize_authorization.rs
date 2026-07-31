//! Server-derived Pod-resize authorization and ceiling clamp. **No Kubernetes.**
//!
//! This is proposal `3i92`'s `0ppk` epic: the authorization and clamp layer, and
//! the fold that applies its verdict. Nothing in *this file* calls Kubernetes,
//! and nothing here may — the only outward edge is [`PodResizeApplier`], whose
//! production implementation is [`crate::resize_lift::ResizeLift`].
//!
//! # The security property: the worker sends no coordinates
//!
//! A worker escalating an invocation continues to send exactly what it has
//! always sent — `LeaseGrantRequest { identity, fencing_token }`. It supplies no
//! namespace, no Pod name, no container name, no container array index and no
//! CPU target, and there is nowhere in the request for it to put one. Every one
//! of those is derived here, server-side, from the durable
//! `BuildPodPermitRow::resize_identity` that was captured against the immutable
//! Pod UID.
//!
//! That matters because the task-run ServiceAccount is deliberately
//! unprivileged: `#2829` granted `pods/resize` to the *controller* only,
//! patch-only and namespaced, and task Pods carry
//! `automountServiceAccountToken: false`. A task run therefore cannot resize any
//! Pod by talking to the API server — but it *can* talk to the coordinator. So
//! "this grant may only ever move this task run's own Pod" is an **application**
//! invariant, and this module is where it lives.
//!
//! # Ownership is decided by the durable row, never by the request
//!
//! [`ResizeAuthority::authorize`] never keys the permit lookup on the task run
//! the caller named. It reads the durable invocation-lease row, recovers the
//! owning task run from the row's **immutable identity** (recorded at queue time
//! and fenced against change by `LeaseIdentityConflict`), and refuses unless the
//! caller's claim matches it. The permit — and therefore the namespace, the Pod
//! name, the container name and the ceiling — is then resolved from the *durable*
//! owner. A caller that names another task run's invocation is refused at step
//! 4, before any permit is read and long before any Kubernetes call could exist.
//!
//! # The clamp
//!
//! The target is `min(configured_leased_millicores, admitted_cpu_millicores)`.
//! The ceiling is the value `g8jk-3` captured from the **stored** Pod after
//! admission, so it already includes whatever a mutating webhook did to the
//! render. Lifting above it would ask the kubelet for CPU the Pod was never
//! admitted for; the clamp is what makes a misconfigured
//! `DJINN_LAUNCHER_LEASED_MILLICORES` a slow build rather than a rejected or
//! evicted Pod.
//!
//! # Where this is reachable from, and where it deliberately is not
//!
//! `0ppk-1b` (#2860) made [`BuildPodPermitRepository`] a production writer: the
//! dispatch seam acquires a permit, binds the Job UID and captures the
//! write-once resize identity before a worker session may start. `0ppk-1c`
//! therefore arms this authority at the one composition site that has those
//! rows — `AppState::new_inner` — via
//! [`crate::build_lease::BuildLeaseService::with_resize_authority`]. Arming it
//! before those rows existed would have degraded every production invocation to
//! `PermitAbsent` on its first grant, which is why 1a shipped it unarmed and
//! said so.
//!
//! [`crate::build_lease::BuildLeaseService`] still holds the authority as an
//! `Option`, and the `None` path is still byte-identical to the pre-`3i92`
//! grant path. That is not vestigial: `DirectServices::with_provider_override`
//! builds a **second** `BuildLeaseService` as an agent-side fallback, and that
//! one **stays unarmed on purpose**. `djinn-agent` cannot depend on the server
//! crate — which is the entire reason `0ppk-1b` introduced the
//! `TaskRunResizeAdmission` trait — so it has no apiserver surface to lift
//! through, and it is not on the path that creates permits either. An unarmed
//! fallback means "no lift": the launcher keeps its birth quota, which is the
//! pre-`3i92` behaviour and is safe. The unsafe direction would be *reporting* a
//! grant no Pod ever received, and that is exactly what an armed-but-surfaceless
//! composition would do.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseRow, BuildLeaseState,
    BuildPodPermitRepository, BuildPodPermitState, BuildPodResizeIdentity,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_supervisor::services::{
    DegradedUnleasedReason, InvocationLiftAuthority, InvocationLiftDecision, LeaseFencingToken,
    LeaseIdentity, LeaseResult, LeaseState, TaskInvocationLeaseIdentity,
};

/// One fully server-derived resize request.
///
/// Every field comes from the durable permit row or from the process's own
/// rendered configuration. There is deliberately no constructor that takes a
/// caller-supplied value: the only way to build one is
/// [`ResizeAuthority::authorize`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodResizeIntent {
    /// The **durable owner's** task run, recovered from the lease row's
    /// immutable identity — never the caller's claim. It is the label the
    /// applier's fresh GET is scoped by.
    pub task_run_id: String,
    /// The **durable owner's** invocation, likewise recovered from the lease
    /// row's immutable identity.
    ///
    /// This is the invocation half of migration 168's
    /// `(task_run_id, pod_uid, resize_invocation_id)` fence. `build_pod_permits`
    /// has PRIMARY KEY `task_run_id`, so one row serves every invocation of a
    /// run — and nothing serializes invocations of one run against each other.
    /// Without this field the applier could not tell its own lift from a
    /// concurrent invocation's.
    pub invocation_id: String,
    /// The permit row's immutable identity, echoed by every durable lifecycle
    /// compare-and-swap the applier makes.
    pub permit_id: String,
    /// The permit's monotonic ownership fence, likewise.
    pub fencing_token: i64,
    /// The permit's resize lifecycle state at authorization time. The applier
    /// refuses to start a lift from anything that is not `birth_confirmed`
    /// (fresh) or `lifted` (idempotent re-confirmation).
    pub permit_state: BuildPodPermitState,
    pub pod_namespace: String,
    pub pod_name: String,
    /// The immutable Pod UID the permit's identity was captured against.
    ///
    /// This is the fence. `PodResizeClient::resize_launcher_cpu` takes only a
    /// Pod *name* and never reads `metadata.uid`, so a Pod deleted and recreated
    /// under the same name is an object it cannot tell apart from the original.
    /// Comparing this against the live `metadata.uid` before any PATCH is the
    /// applier's job, and it is why this field is carried on the intent rather
    /// than re-derived.
    pub pod_uid: String,
    pub launcher_container_name: String,
    /// `status.initContainerStatuses[..].containerID` as captured. A launcher
    /// restart replaces it, and a restarted launcher invalidates the lift: the
    /// cgroup the target was reasoned about is gone.
    pub launcher_container_id: String,
    /// The protocol the server resolved for this Pod. A live Pod that declares
    /// a different one is a hard failure — exactly one authority governs an
    /// admitted Pod.
    pub effective_launcher_protocol: LauncherAuthorityProtocol,
    /// Already clamped to the stored ceiling. See [`ResizeAuthority::authorize`].
    pub target_millicores: i64,
    /// The ceiling this target was clamped against, carried for the assertion
    /// that `target_millicores <= admitted_cpu_millicores` can be made on the
    /// intent itself rather than on a value a test had to re-derive.
    pub admitted_cpu_millicores: i64,
}

/// Why an authorized resize did not take.
///
/// Carries a settled [`DegradedUnleasedReason`] plus a rendered detail for the
/// log line. There is no "retry me" variant, and there cannot be one: see
/// [`LeaseResult::DegradedUnleased`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeApplyFailure {
    /// The closed reason that reaches the caller.
    pub reason: DegradedUnleasedReason,
    /// What actually happened, for the log line and the test message.
    pub detail: String,
}

impl ResizeApplyFailure {
    /// Build a failure.
    #[must_use]
    pub fn new(reason: DegradedUnleasedReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

/// The one outward edge of this module, and the seam that reaches Kubernetes.
///
/// # Why this is `async` and returns a `Result`
///
/// `0ppk-1a` shipped this as `fn record(&self, intent: &PodResizeIntent)` —
/// synchronous, infallible, returning unit. That shape was right while the far
/// side was a counter, and is **structurally wrong** now that the far side is a
/// `pods/resize` PATCH followed by a status-confirmation poll. A fallible
/// operation plugged into an infallible seam cannot report failure, so
/// [`ResizeAuthority::fold_into_grant`] would have no failure to map, every
/// uncertainty test below would pass without exercising anything, and the epic
/// would collect another green, merged, inert slice.
///
/// So: reverting this trait to `fn record(&self, &PodResizeIntent)` is the named
/// mutation for acceptance criterion 2. It does not merely fail a test — it
/// stops `resize_lift.rs` and every uncertainty case in
/// `resize_lift_tests.rs` from compiling, because there is nowhere left to put
/// the failure.
///
/// It also keeps "zero Kubernetes calls" assertable on a **counter** rather than
/// on a returned error: a refusal that had already emitted a PATCH would return
/// exactly the same `DegradedUnleased` as one that emitted none.
#[async_trait::async_trait]
pub trait PodResizeApplier: Send + Sync {
    /// Apply one authorized, already-clamped resize.
    ///
    /// Called exactly once per authorized resize, and never for a refusal.
    /// Returns `Ok(())` **only** on a status-confirmed apply.
    ///
    /// # Errors
    ///
    /// [`ResizeApplyFailure`] for every way the lift can fail to take.
    async fn apply(&self, intent: &PodResizeIntent) -> Result<(), ResizeApplyFailure>;
}

/// An applier that only counts, and always succeeds. Used by this crate's
/// authorization tests and by any composition that wants the authorization
/// decision without the PATCH.
///
/// It is deliberately NOT the production applier: it proves *which* intents
/// reached the boundary and with what target, and proves nothing about whether
/// a Pod moved. `resize_lift.rs` owns that, against fixtures whose confirmation
/// rule is the production one.
#[derive(Debug, Default)]
pub struct CountingPodResizeApplier {
    intents: std::sync::Mutex<Vec<PodResizeIntent>>,
    calls: AtomicU64,
}

impl CountingPodResizeApplier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times the Kubernetes boundary was reached. This is the
    /// "zero Kubernetes calls" counter.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Acquire)
    }

    /// Every intent recorded, in order.
    #[must_use]
    pub fn intents(&self) -> Vec<PodResizeIntent> {
        self.intents
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// How many recorded intents asked for more CPU than the ceiling they were
    /// clamped against. This is acceptance criterion 2's counter, and it is
    /// computed from the intent itself rather than from a value the test
    /// restated, so removing the clamp cannot leave it reading zero.
    #[must_use]
    pub fn intents_above_ceiling(&self) -> usize {
        self.intents()
            .iter()
            .filter(|intent| intent.target_millicores > intent.admitted_cpu_millicores)
            .count()
    }
}

#[async_trait::async_trait]
impl PodResizeApplier for CountingPodResizeApplier {
    async fn apply(&self, intent: &PodResizeIntent) -> Result<(), ResizeApplyFailure> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut guard) = self.intents.lock() {
            guard.push(intent.clone());
        }
        Ok(())
    }
}

/// An applier that counts and then fails with a fixed reason. Exists so a test
/// can prove that an apply failure reaches the caller as
/// [`LeaseResult::DegradedUnleased`] *without* standing up a Pod fixture.
#[derive(Debug)]
pub struct FailingPodResizeApplier {
    inner: CountingPodResizeApplier,
    failure: ResizeApplyFailure,
}

impl FailingPodResizeApplier {
    /// Fail every apply with `failure`.
    #[must_use]
    pub fn new(failure: ResizeApplyFailure) -> Self {
        Self {
            inner: CountingPodResizeApplier::new(),
            failure,
        }
    }

    /// How many times the Kubernetes boundary was reached.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.inner.calls()
    }
}

#[async_trait::async_trait]
impl PodResizeApplier for FailingPodResizeApplier {
    async fn apply(&self, intent: &PodResizeIntent) -> Result<(), ResizeApplyFailure> {
        let _ = self.inner.apply(intent).await;
        Err(self.failure.clone())
    }
}

/// Whether a refusal is an authorization denial or an uncertainty.
///
/// Both refuse, both emit zero intents and both surface to the caller as
/// [`LeaseResult::DegradedUnleased`]. They are kept apart because a denial means
/// a caller asked for something that is not its own — which is worth a `WARN`
/// and, eventually, an alert — while an uncertainty means this server could not
/// prove the resize is allowed, which is an operational fact about the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalClass {
    Denied,
    Uncertain,
}

/// A refusal, with the reason that reaches the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeRefusal {
    pub class: RefusalClass,
    pub reason: DegradedUnleasedReason,
}

impl ResizeRefusal {
    const fn denied(reason: DegradedUnleasedReason) -> Self {
        Self {
            class: RefusalClass::Denied,
            reason,
        }
    }
    const fn uncertain(reason: DegradedUnleasedReason) -> Self {
        Self {
            class: RefusalClass::Uncertain,
            reason,
        }
    }
}

/// Everything this layer can decide about one grant.
///
/// There is no `Err` here on purpose. See [`LeaseResult::DegradedUnleased`] for
/// why an uncertainty on this path must not be expressible as something a caller
/// could classify as retryable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResizeAuthorizationOutcome {
    /// Server-derived and clamped. The only variant `0ppk-1b` may PATCH.
    ///
    /// Boxed, and that is not lint appeasement. This outcome is held across an
    /// `.await` inside `BuildLeaseService::grant`, which is itself held inside
    /// the dispatch rules engine's future. Inlining a ~224-byte intent there
    /// grew that future past a tokio worker's 2MiB stack and aborted
    /// `rules::tests::amend_while_building_dispatches_one_reconcile_task_from_event`
    /// with a stack overflow — a failure with no textual connection to resize at
    /// all. One pointer keeps the whole chain bounded no matter how the intent
    /// grows later.
    Authorized(Box<PodResizeIntent>),
    /// Refused. Zero intents were recorded.
    Refused(ResizeRefusal),
    /// The durable authority is not armed to enforce, so no invocation is lifted
    /// at all and there is nothing to authorize.
    ///
    /// Deliberately NOT a refusal: `Shadow` clamps by design and `Unleased`
    /// means the leaf was born with no quota of its own, so neither is a
    /// degrade — the lease result is passed through untouched. It records zero
    /// intents like a refusal does.
    NotLifting(InvocationLiftDecision),
}

/// The server-side resize authorization.
///
/// # Should we lift at all?
///
/// That predicate is [`InvocationLiftDecision`] — the agent/launcher-side
/// projection of the durable invocation-lease authority — read through the
/// injected [`InvocationLiftAuthority`], and the Pod's own
/// [`LauncherAuthorityProtocol`]. Both are TYPES. This module never reads the
/// retired handoff-protocol columns that `9oga`'s `flc5` will DROP, which is
/// what lets it survive that migration unchanged;
/// `scripts/check-resize-authorization-boundary.sh` is the CI assertion that
/// keeps it true.
pub struct ResizeAuthority {
    leases: Arc<BuildLeaseRepository>,
    permits: Arc<BuildPodPermitRepository>,
    lift: Arc<dyn InvocationLiftAuthority>,
    /// `DJINN_LAUNCHER_LEASED_MILLICORES` as this process rendered it —
    /// `djinn_k8s::launcher_cpu::launcher_leased_millicores(config)`. Passed as
    /// a plain number rather than a `KubernetesConfig` so this module has no
    /// Kubernetes dependency at all, in either direction.
    configured_leased_millicores: i64,
    applier: Arc<dyn PodResizeApplier>,
}

impl ResizeAuthority {
    #[must_use]
    pub fn new(
        leases: Arc<BuildLeaseRepository>,
        permits: Arc<BuildPodPermitRepository>,
        lift: Arc<dyn InvocationLiftAuthority>,
        configured_leased_millicores: i64,
        applier: Arc<dyn PodResizeApplier>,
    ) -> Self {
        Self {
            leases,
            permits,
            lift,
            configured_leased_millicores,
            applier,
        }
    }

    /// Authorize one grant, deriving every coordinate server-side.
    ///
    /// The order of the steps is the security property, not a style choice:
    /// ownership is settled from durable state **before** a permit is read, and
    /// no code path can reach [`PodResizeIntentSink::record`] without having
    /// passed it.
    ///
    /// 1. Should we lift at all? Only [`InvocationLiftDecision::Lift`] proceeds.
    /// 2. Read the durable invocation-lease row for the named invocation.
    /// 3. Fence it against the presented token.
    /// 4. Recover the OWNING task run from the row's immutable identity and
    ///    refuse unless the caller's claim matches, in full.
    /// 5. Resolve the permit from the **durable owner's** task-run id.
    /// 6. Require a captured write-once resize identity on a resizable protocol.
    /// 7. Clamp the target to the stored ceiling and emit the intent.
    ///
    /// It deliberately performs **no** Kubernetes call. Applying the authorized
    /// intent is [`Self::fold_into_grant`]'s job, so that an apply failure is a
    /// property of the grant result rather than something this function would
    /// have to smuggle back through an outcome that has no error arm.
    pub async fn authorize(
        &self,
        claim: &TaskInvocationLeaseIdentity,
        fencing_token: &LeaseFencingToken,
    ) -> ResizeAuthorizationOutcome {
        let decision = self.lift.invocation_lift_decision().await;
        if decision != InvocationLiftDecision::Lift {
            return ResizeAuthorizationOutcome::NotLifting(decision);
        }

        let row = match self
            .leases
            .get(&BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
                consumer_id: claim.invocation_id.clone(),
            })
            .await
        {
            Ok(Some(row)) => row,
            // An invocation with no durable row is not a resizable subject. It
            // is an uncertainty rather than a denial: the row may simply have
            // been terminalized between the grant and this read.
            Ok(None) => {
                return Self::refuse(ResizeRefusal::uncertain(
                    DegradedUnleasedReason::PermitAbsent,
                ));
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    invocation_id = %claim.invocation_id,
                    "resize authorization: durable lease row unreadable; degrading unleased"
                );
                return Self::refuse(ResizeRefusal::uncertain(
                    DegradedUnleasedReason::AuthorizationUnreadable,
                ));
            }
        };

        if row.fencing_token != Some(fencing_token.0 as i64) {
            return Self::refuse(ResizeRefusal::denied(
                DegradedUnleasedReason::FencingTokenMismatch,
            ));
        }

        let Some(owner) = durable_owner(&row) else {
            tracing::error!(
                identity = %row.immutable_identity,
                "resize authorization: durable immutable identity is unparseable; degrading unleased"
            );
            return Self::refuse(ResizeRefusal::uncertain(
                DegradedUnleasedReason::AuthorizationUnreadable,
            ));
        };
        if owner != *claim {
            tracing::warn!(
                claimed_task_run_id = %claim.task_run_id,
                owning_task_run_id = %owner.task_run_id,
                invocation_id = %claim.invocation_id,
                "resize authorization DENIED: the caller does not own the invocation it named"
            );
            return Self::refuse(ResizeRefusal::denied(
                DegradedUnleasedReason::NotTheInvocationOwner,
            ));
        }

        // From here on the task-run id is the DURABLE owner's, never the claim's.
        // They are equal at this point; using `owner` keeps that a property of
        // the code rather than of the comparison two lines above.
        let permit = match self.permits.active(&owner.task_run_id).await {
            Ok(Some(permit)) => permit,
            Ok(None) => {
                return Self::refuse(ResizeRefusal::uncertain(
                    DegradedUnleasedReason::PermitAbsent,
                ));
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    task_run_id = %owner.task_run_id,
                    "resize authorization: build-pod permit unreadable; degrading unleased"
                );
                return Self::refuse(ResizeRefusal::uncertain(
                    DegradedUnleasedReason::AuthorizationUnreadable,
                ));
            }
        };
        let permit_id = permit.permit_id.clone();
        let permit_fence = permit.fencing_token;
        let permit_state = permit.state;
        let Some(identity) = permit.resize_identity else {
            return Self::refuse(ResizeRefusal::uncertain(
                DegradedUnleasedReason::ResizeIdentityUnknown,
            ));
        };

        match clamp(
            &owner.task_run_id,
            &owner.invocation_id,
            &permit_id,
            permit_fence,
            permit_state,
            &identity,
            self.configured_leased_millicores,
        ) {
            Ok(intent) => ResizeAuthorizationOutcome::Authorized(Box::new(intent)),
            Err(refusal) => Self::refuse(refusal),
        }
    }

    /// Every refusal funnels through here, and no refusal touches the sink.
    const fn refuse(refusal: ResizeRefusal) -> ResizeAuthorizationOutcome {
        ResizeAuthorizationOutcome::Refused(refusal)
    }

    /// Fold the authorization into the lease result a grant returns.
    ///
    /// Called from [`crate::build_lease::BuildLeaseService::grant`]. When no
    /// authority is composed — which is every composition on `main`, see this
    /// module's header — the grant result is returned byte-for-byte unchanged.
    pub async fn fold_into_grant(
        authority: Option<&Self>,
        identity: &LeaseIdentity,
        fencing_token: &LeaseFencingToken,
        granted: LeaseResult,
    ) -> LeaseResult {
        let (Some(authority), LeaseIdentity::TaskInvocation(claim)) = (authority, identity) else {
            return granted;
        };
        // A grant that did not actually take is not a resize question. Only a
        // durable state that this token really owns is worth authorizing, and
        // this mirrors the acceptance test the worker itself applies in
        // `djinn_agent::process` before it lifts.
        let took = matches!(
            &granted,
            LeaseResult::Status(status)
                if matches!(
                    status.state,
                    LeaseState::Launching | LeaseState::Bound | LeaseState::Active
                ) && status.fencing_token.as_ref() == Some(fencing_token)
        );
        if !took {
            return granted;
        }
        match authority.authorize(claim, fencing_token).await {
            // THE LIFT. `0ppk-1a` returned `granted` here and dropped the
            // authorized intent on the floor, which made the whole
            // authorization layer a decision nobody acted on. Restoring that
            // passthrough is acceptance criterion 2's second named mutation:
            // the happy-path PATCH counter goes to zero and every
            // status-confirmation test in `resize_lift_tests.rs` stops proving
            // anything about the grant path.
            ResizeAuthorizationOutcome::Authorized(intent) => {
                match authority.applier.apply(&intent).await {
                    Ok(()) => granted,
                    Err(failure) => {
                        tracing::warn!(
                            task_run_id = %intent.task_run_id,
                            pod_uid = %intent.pod_uid,
                            target_millicores = intent.target_millicores,
                            reason = ?failure.reason,
                            detail = %failure.detail,
                            "resize lift did not take; degrading unleased"
                        );
                        LeaseResult::DegradedUnleased {
                            reason: failure.reason,
                        }
                    }
                }
            }
            ResizeAuthorizationOutcome::NotLifting(_) => granted,
            ResizeAuthorizationOutcome::Refused(refusal) => LeaseResult::DegradedUnleased {
                reason: refusal.reason,
            },
        }
    }
}

/// Recover the task run that durably owns an invocation lease.
///
/// The immutable identity is written once by
/// [`crate::build_lease`]'s `identity()` as `task:<task>:<run>:<invocation>` and
/// is fenced against change by `LeaseIdentityConflict`, so it — not the request
/// — is the record of who owns the row. Ids are UUIDs and carry no `:`, and the
/// invocation segment is taken as the remainder so a malformed tail cannot be
/// silently truncated into a match.
fn durable_owner(row: &BuildLeaseRow) -> Option<TaskInvocationLeaseIdentity> {
    if row.state == BuildLeaseState::Terminal {
        return None;
    }
    let mut parts = row.immutable_identity.splitn(4, ':');
    let ("task", Some(task_id), Some(task_run_id), Some(invocation_id)) =
        (parts.next()?, parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if task_id.is_empty() || task_run_id.is_empty() || invocation_id.is_empty() {
        return None;
    }
    Some(TaskInvocationLeaseIdentity {
        task_id: task_id.to_string(),
        task_run_id: task_run_id.to_string(),
        invocation_id: invocation_id.to_string(),
    })
}

/// The clamp: `min(configured, admitted)`, on a resizable protocol only.
///
/// Split out as a free function so the ceiling arithmetic is testable without a
/// database, and so acceptance criterion 2's mutation ("remove the clamp") is a
/// one-line edit at a single site rather than something that could be half-done.
fn clamp(
    task_run_id: &str,
    invocation_id: &str,
    permit_id: &str,
    fencing_token: i64,
    permit_state: BuildPodPermitState,
    identity: &BuildPodResizeIdentity,
    configured_leased_millicores: i64,
) -> Result<PodResizeIntent, ResizeRefusal> {
    // `leaf-v1` renders no launcher `limits.cpu` at all, so a container limit
    // there would be an ancestor clamp over every process in the Pod. Parsing
    // through the shared protocol type rather than comparing string literals is
    // what keeps this agreeing with migration 164's CHECK constraint.
    let protocol: LauncherAuthorityProtocol = identity
        .effective_launcher_protocol
        .parse()
        .map_err(|_| ResizeRefusal::uncertain(DegradedUnleasedReason::ProtocolNotResizable))?;
    if protocol != LauncherAuthorityProtocol::ResizeV2 {
        return Err(ResizeRefusal::uncertain(
            DegradedUnleasedReason::ProtocolNotResizable,
        ));
    }
    let ceiling = identity.admitted_cpu_millicores;
    if ceiling <= 0 || configured_leased_millicores <= 0 {
        return Err(ResizeRefusal::uncertain(
            DegradedUnleasedReason::CeilingUnusable,
        ));
    }
    // THE CLAMP. Removing the `.min(ceiling)` here is acceptance criterion 2's
    // stated mutation, and it must make `intents_above_ceiling()` report 1.
    let target_millicores = configured_leased_millicores.min(ceiling);
    Ok(PodResizeIntent {
        task_run_id: task_run_id.to_owned(),
        invocation_id: invocation_id.to_owned(),
        permit_id: permit_id.to_owned(),
        fencing_token,
        permit_state,
        pod_namespace: identity.pod_namespace.clone(),
        pod_name: identity.pod_name.clone(),
        pod_uid: identity.pod_uid.clone(),
        launcher_container_name: identity.launcher_container_name.clone(),
        launcher_container_id: identity.launcher_container_id.clone(),
        effective_launcher_protocol: protocol,
        target_millicores,
        admitted_cpu_millicores: ceiling,
    })
}
