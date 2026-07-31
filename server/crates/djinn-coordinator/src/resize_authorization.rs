//! Server-derived Pod-resize authorization and ceiling clamp. **No Kubernetes.**
//!
//! This is the first slice of proposal `3i92`'s `0ppk` epic: the authorization
//! and clamp layer that `0ppk-1b`'s `pods/resize` PATCH path will sit on top of.
//! Nothing here calls Kubernetes, and nothing here may — the only outward edge
//! is [`PodResizeIntentSink`], which `0ppk-1b` replaces with the real
//! limits-only resize client.
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
//! # What is NOT reachable yet, deliberately
//!
//! [`BuildPodPermitRepository`] still has no production caller on `main`:
//! neither `acquire` nor `bind_or_refresh_job_uid` is called from `server/src`
//! or `runtime::prepare`. So no permit row exists in production, and
//! [`ResizeAuthority`] is consequently **not composed into `AppState`** by this
//! slice — [`crate::build_lease::BuildLeaseService`] holds it as an `Option` and
//! behaves exactly as it does today when it is `None`. Wiring the authority and
//! the permit's creation together is `0ppk-1b`'s job. Composing it here, against
//! a relation nothing writes, would degrade every production invocation to
//! unleased on its first grant.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseRow, BuildLeaseState,
    BuildPodPermitRepository, BuildPodResizeIdentity,
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
    pub pod_namespace: String,
    pub pod_name: String,
    /// The immutable Pod UID the permit's identity was captured against. Carried
    /// so `0ppk-1b` can fence its PATCH on the object it believes it is moving —
    /// Pod *names* are reused, UIDs are not.
    pub pod_uid: String,
    pub launcher_container_name: String,
    /// Already clamped to the stored ceiling. See [`ResizeAuthority::authorize`].
    pub target_millicores: i64,
    /// The ceiling this target was clamped against, carried for the assertion
    /// that `target_millicores <= admitted_cpu_millicores` can be made on the
    /// intent itself rather than on a value a test had to re-derive.
    pub admitted_cpu_millicores: i64,
}

/// The one outward edge of this module, and the seam `0ppk-1b` replaces.
///
/// This exists so "zero Kubernetes calls" is assertable on a **counter** rather
/// than on a returned error. A refusal that still emitted an intent would return
/// the same `DegradedUnleased` as a refusal that emitted none, and only the
/// counter can tell those apart — which is the whole failure mode acceptance
/// criterion 1 is written against.
pub trait PodResizeIntentSink: Send + Sync {
    /// Called exactly once per authorized resize, and never for a refusal.
    fn record(&self, intent: &PodResizeIntent);
}

/// A sink that only counts. Used by this crate's tests and by any composition
/// that wants the authorization decision without the PATCH.
#[derive(Debug, Default)]
pub struct CountingPodResizeIntentSink {
    intents: std::sync::Mutex<Vec<PodResizeIntent>>,
    calls: AtomicU64,
}

impl CountingPodResizeIntentSink {
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

impl PodResizeIntentSink for CountingPodResizeIntentSink {
    fn record(&self, intent: &PodResizeIntent) {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut guard) = self.intents.lock() {
            guard.push(intent.clone());
        }
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
    Authorized(PodResizeIntent),
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
    sink: Arc<dyn PodResizeIntentSink>,
}

impl ResizeAuthority {
    #[must_use]
    pub fn new(
        leases: Arc<BuildLeaseRepository>,
        permits: Arc<BuildPodPermitRepository>,
        lift: Arc<dyn InvocationLiftAuthority>,
        configured_leased_millicores: i64,
        sink: Arc<dyn PodResizeIntentSink>,
    ) -> Self {
        Self {
            leases,
            permits,
            lift,
            configured_leased_millicores,
            sink,
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
        let Some(identity) = permit.resize_identity else {
            return Self::refuse(ResizeRefusal::uncertain(
                DegradedUnleasedReason::ResizeIdentityUnknown,
            ));
        };

        match clamp(&identity, self.configured_leased_millicores) {
            Ok(intent) => {
                self.sink.record(&intent);
                ResizeAuthorizationOutcome::Authorized(intent)
            }
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
            ResizeAuthorizationOutcome::Authorized(_)
            | ResizeAuthorizationOutcome::NotLifting(_) => granted,
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
        pod_namespace: identity.pod_namespace.clone(),
        pod_name: identity.pod_name.clone(),
        pod_uid: identity.pod_uid.clone(),
        launcher_container_name: identity.launcher_container_name.clone(),
        target_millicores,
        admitted_cpu_millicores: ceiling,
    })
}
