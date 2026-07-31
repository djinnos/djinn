//! The status-confirmed lift, proved against a real Postgres and a stored Pod.
//!
//! # What is real in here, and what is not
//!
//! Real: the permit relation (`build_pod_permits` over an ephemeral, template-
//! cloned Postgres — `open_in_memory` is not an in-memory fake), the lease FIFO,
//! the durable invocation-lift authority, `ResizeAuthority`, `BuildLeaseService`
//! and its grant path, and — through
//! [`djinn_k8s::pod_resize_fixture::StoredTaskRunPod`] — the production
//! observation (`observe_launcher_sidecar`), the production PATCH
//! (`PodResizeClient`, strategic-merge semantics for `initContainers`) and the
//! production confirmation rule (`confirm_launcher_cpu`, millicores, init
//! statuses only).
//!
//! Fake: exactly one thing, the HTTP transport. That is the fault-injection
//! seam the task specifies, and it is the only place a fake belongs — a fake
//! permit repository would agree with every assertion below and prove none of
//! them.
//!
//! # What stays green if the lift's body does nothing?
//!
//! Nothing. Every assertion here reads either the Pod's
//! `status.initContainerStatuses` (the only field that can confirm a resize) or
//! the durable permit's lifecycle state after the fact, and the happy-path tests
//! assert a **non-zero** PATCH counter so a no-op cannot pass by refusing
//! everything.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_db::{
    AcquireBuildPodPermitResult, BuildLeaseRepository, BuildPodPermitRepository,
    BuildPodPermitState, BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, Database,
    InvocationLeaseAuthorityRepository, InvocationLeaseMode,
};
use djinn_k8s::pod_resize::PodResizeError;
use djinn_k8s::pod_resize_fixture::{ApiFault, StoredTaskRunPod};
use djinn_k8s::runtime::{LauncherObservationError, ObservedLauncherSidecar};
use djinn_supervisor::services::{
    DegradedUnleasedReason, DurableInvocationLiftAuthority, InvocationLiftAuthority,
    LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest,
    LeaseResult, TaskInvocationLeaseIdentity,
};

use crate::build_lease::BuildLeaseService;
use crate::resize_authorization::{PodResizeApplier, ResizeAuthority};
use crate::resize_lift::{LauncherResizeSurface, ResizeLift};

const CONFIGURED_CAP: i64 = 9;
/// `DJINN_LAUNCHER_LEASED_MILLICORES` as the DEPLOYMENT DEFAULT render sets it.
///
/// `gvix` removed this from the lift's inputs entirely — no CPU quantity
/// reaches `ResizeAuthority` from process configuration any more. It survives
/// as the number the per-Pod ceilings below are placed on either side of, which
/// is what makes "the Pod's own admitted value" distinguishable from "the
/// fleet-wide default" in the assertions.
const DEPLOYMENT_DEFAULT_LEASED: u64 = 4_000;
/// What the fixture Pod was admitted with. Deliberately BELOW
/// [`DEPLOYMENT_DEFAULT_LEASED`] so a target taken from the default would be a
/// visibly different number.
const ADMITTED_CEILING: u64 = 2_500;
/// `djinn_server::task_run_resize_bootstrap::BIRTH_CPU_MILLICORES`. Restated
/// rather than imported because `djinn-server` depends on this crate, not the
/// other way round. Nothing here asserts a *policy* about the birth limit — it
/// is only the "somewhere other than the target" the lift has to move the
/// launcher away from, and any value below the ceiling would serve.
const BIRTH_MILLICORES: u64 = 250;

const RUN: &str = "01983f00-0000-7000-8000-00000000d001";
const TASK: &str = "01983f00-0000-7000-8000-00000000e001";
const INVOCATION: &str = "01983f00-0000-7000-8000-00000000f001";
const POD_UID: &str = "pod-uid-original";

fn identity() -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: TASK.into(),
        task_run_id: RUN.into(),
        invocation_id: INVOCATION.into(),
    })
}

// ── The one fake: the transport ────────────────────────────────────────────

/// Adapts a stored Pod to the lift's surface. Both methods delegate straight
/// into the production code paths inside the fixture; this type adds no rules
/// of its own, which is what stops the tests from validating a re-implementation.
struct FixtureSurface(StoredTaskRunPod);

#[async_trait]
impl LauncherResizeSurface for FixtureSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        self.0.observe_launcher()
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
        self.0
            .resize_launcher_cpu(pod_name, target_millicores)
            .await
    }
}

// ── The live composition ───────────────────────────────────────────────────

/// Wraps the REAL [`ResizeLift`] and records the intent it was handed.
///
/// It changes nothing about the lift — it delegates — so the recorded intent is
/// the one production would apply. That matters for the ceiling-provenance
/// assertion: `admitted_cpu_millicores` is the bound the PATCH was clamped
/// against, and reading it here reads the value that BOUND the request rather
/// than a value a test wrote down and read back.
struct RecordingApplier {
    inner: ResizeLift,
    intents: std::sync::Mutex<Vec<crate::resize_authorization::PodResizeIntent>>,
}

#[async_trait]
impl PodResizeApplier for RecordingApplier {
    async fn apply(
        &self,
        intent: &crate::resize_authorization::PodResizeIntent,
    ) -> Result<(), crate::resize_authorization::ResizeApplyFailure> {
        if let Ok(mut guard) = self.intents.lock() {
            guard.push(intent.clone());
        }
        self.inner.apply(intent).await
    }
}

struct Fixture {
    service: Arc<BuildLeaseService>,
    permits: Arc<BuildPodPermitRepository>,
    pod: StoredTaskRunPod,
    applier: Arc<RecordingApplier>,
    _db: Database,
}

impl Fixture {
    /// Armed authority, a healthy `resize-v2` Pod, a permit whose write-once
    /// identity was captured **from that Pod's own observation** rather than
    /// hand-written — so a fence comparison cannot pass because a test typed the
    /// same string twice.
    async fn armed(pod: StoredTaskRunPod, budget: Duration) -> Self {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        seed_task_run(&db, RUN).await;

        let lease_authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
        let seeded = lease_authority.seed_baseline().await.unwrap();
        lease_authority
            .set_mode_and_cap(
                seeded.epoch,
                InvocationLeaseMode::Enforce,
                Some(CONFIGURED_CAP),
            )
            .await
            .unwrap();

        let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
        let permits = Arc::new(BuildPodPermitRepository::new(db.clone()));
        let lift: Arc<dyn InvocationLiftAuthority> = Arc::new(DurableInvocationLiftAuthority::new(
            db.clone(),
            "resize-lift-tests",
        ));
        let applier = Arc::new(RecordingApplier {
            inner: ResizeLift::with_surface(
                Arc::clone(&permits),
                Arc::new(FixtureSurface(pod.clone())),
            )
            .with_wait(budget, Duration::from_millis(1)),
            intents: std::sync::Mutex::new(Vec::new()),
        });
        let authority = Arc::new(ResizeAuthority::new(
            Arc::clone(&leases),
            Arc::clone(&permits),
            lift,
            Arc::clone(&applier) as Arc<dyn PodResizeApplier>,
        ));
        let service = Arc::new(
            BuildLeaseService::new(Arc::clone(&leases), CONFIGURED_CAP)
                .with_invocation_lease_authority(Arc::clone(&lease_authority))
                .with_resize_authority(authority),
        );
        assert!(matches!(service.recover().await, LeaseResult::Status(_)));

        let fixture = Self {
            service,
            permits,
            pod,
            applier,
            _db: db,
        };
        fixture.seed_permit().await;
        fixture.birth_downsize().await;
        fixture
    }

    /// The birth downsize `0ppk-1b` performs before the worker session may
    /// dispatch: the launcher is moved from its admitted ceiling to
    /// [`BIRTH_MILLICORES`], and the captured ceiling stays on the durable row.
    ///
    /// Without this the fixture Pod would already be holding the lift's target
    /// and every confirmation would pass trivially — a "never actuates" node
    /// would still confirm, because there would be nothing to actuate. It is
    /// exactly the shape of vacuous pass this epic keeps producing, so it is
    /// asserted rather than assumed: the launcher's INIT status must read 250m
    /// before any lift runs.
    async fn birth_downsize(&self) {
        let pod_name = format!("taskrun-{POD_UID}");
        self.pod
            .resize_launcher_cpu(&pod_name, BIRTH_MILLICORES)
            .await
            .expect("the birth downsize confirms on a healthy node");
        assert_eq!(
            self.pod.launcher_status_cpu().as_deref(),
            Some(format!("{BIRTH_MILLICORES}m").as_str()),
            "the launcher must sit at its birth limit before a lift, or the lift              has nothing to move and every confirmation passes vacuously"
        );
        // From here on, a PATCH counter reading zero means "the lift issued
        // none", not "nothing has ever happened".
        self.pod.reset_patch_counter();
    }

    /// The two writes `0ppk-1b` makes on the live dispatch path.
    async fn seed_permit(&self) {
        let AcquireBuildPodPermitResult::Acquired { row, .. } =
            self.permits.acquire(RUN, CONFIGURED_CAP).await
        else {
            panic!("the permit pool must admit the run");
        };
        self.permits
            .bind_or_refresh_job_uid(RUN, &row.permit_id, row.fencing_token, "job-uid")
            .await
            .expect("binding the Job UID must succeed");
        let observed = self
            .pod
            .observe_launcher()
            .expect("the fixture Pod observes")
            .expect("the fixture Pod exists");
        let identity = BuildPodResizeIdentity {
            pod_namespace: observed.namespace,
            pod_name: observed.pod_name,
            pod_uid: observed.pod_uid,
            launcher_container_name: observed.launcher_container_name,
            launcher_container_id: observed
                .launcher_container_id
                .expect("the launcher has started"),
            image_digest: observed.image_digest.expect("the launcher has an image id"),
            observed_launcher_protocol: observed
                .observed_protocol
                .clone()
                .expect("the launcher declares a protocol"),
            effective_launcher_protocol: observed
                .observed_protocol
                .expect("the launcher declares a protocol"),
            admitted_cpu_millicores: i64::try_from(
                observed
                    .admitted_cpu_millicores
                    .expect("resize-v2 renders a ceiling"),
            )
            .unwrap(),
        };
        let captured = self
            .permits
            .capture_resize_identity(RUN, &row.permit_id, row.fencing_token, &identity)
            .await
            .unwrap();
        assert!(
            matches!(captured, CaptureBuildPodResizeIdentityResult::Captured(_)),
            "the write-once resize identity must capture: {captured:?}"
        );
    }

    async fn escalate(&self) -> LeaseFencingToken {
        match self
            .service
            .queue(LeaseQueueRequest {
                identity: identity(),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await
        {
            LeaseResult::Granted(grant) => grant.fencing_token,
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    /// Escalate and acknowledge — the call that authorizes AND applies.
    async fn lift(&self) -> LeaseResult {
        let token = self.escalate().await;
        self.service
            .grant(LeaseGrantRequest {
                identity: identity(),
                fencing_token: token,
            })
            .await
    }

    /// Every intent that reached the applier, in order.
    fn intents(&self) -> Vec<crate::resize_authorization::PodResizeIntent> {
        self.applier
            .intents
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    async fn permit_state(&self) -> BuildPodPermitState {
        self.permits
            .active(RUN)
            .await
            .expect("the permit is readable")
            .expect("the permit exists")
            .state
    }
}

async fn seed_task_run(db: &Database, run: &str) {
    let project_id = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::seed_project(db, &project_id, &format!("proj-{project_id}")).await;
    let task_id = djinn_db::test_support::seed_task_row(
        db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &project_id,
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    djinn_db::TaskRunRepository::new(db.clone())
        .create(djinn_db::CreateTaskRunParams {
            id: run,
            project_id: &project_id,
            task_id: &task_id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed the task run the permit is keyed on");
}

fn healthy_pod() -> StoredTaskRunPod {
    pod_admitted_at(ADMITTED_CEILING)
}

/// A healthy `resize-v2` Pod whose launcher was admitted with `ceiling`.
fn pod_admitted_at(ceiling: u64) -> StoredTaskRunPod {
    StoredTaskRunPod::resize_v2(POD_UID, &format!("{ceiling}m"))
}

/// No confirmation budget: one observation, no waiting. Used by every case that
/// wants the SPECIFIC thing the status reported rather than the fact that a
/// budget was spent.
const NO_WAIT: Duration = Duration::ZERO;

// ── AC3 / AC2: a grant only after the init-container status agrees ──────────

/// **ACCEPTANCE CRITERION 3 (positive) and CRITERION 2 (non-vacuity).**
///
/// The lift lands, and it lands *in the field that can confirm it*: the
/// launcher's `status.initContainerStatuses` entry, in millicores. The PATCH
/// counter is asserted NON-ZERO, which is what stops criterion 2's second
/// mutation — restoring `Authorized(_) => granted` in `fold_into_grant` — from
/// passing: with the passthrough restored, nothing is patched at all.
#[tokio::test]
async fn a_status_confirmed_lift_grants_and_records_lifted() {
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{BIRTH_MILLICORES}m").as_str()),
        "precondition: the launcher sits at its birth limit, NOT at the target"
    );

    let result = fixture.lift().await;

    assert!(
        matches!(result, LeaseResult::Status(_)),
        "a confirmed lift returns the lease status unchanged: {result:?}"
    );
    assert!(
        fixture.pod.resize_patches() > 0,
        "the happy path must actually PATCH; a zero counter here is the shipped \
         `Authorized(_) => granted` passthrough coming back"
    );
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{ADMITTED_CEILING}m").as_str()),
        "the launcher's INIT-container status is what confirms, and it must have \
         MOVED from {BIRTH_MILLICORES}m to the clamped target"
    );
    assert_eq!(
        fixture.permit_state().await,
        BuildPodPermitState::Lifted,
        "a confirmed lift advances the durable lifecycle to `lifted`"
    );
}

/// **ACCEPTANCE CRITERION 3, the required distinct fixture.**
///
/// `status.containerStatuses` already carries a `cgroup-launcher` entry holding
/// exactly the target, while `status.initContainerStatuses` never actuates. A
/// confirmation that read the wrong array — or that accepted the PATCH response
/// or the mutated `spec` — would report success here. It must degrade.
///
/// NAMED FAILING MUTATION: make `confirm_launcher_cpu` fall back to
/// `locate_launcher_status`'s regular-container sibling, or have `ResizeLift`
/// return `Ok(())` on `resize_launcher_cpu`'s accepted PATCH instead of on its
/// confirmation, and this test grants.
#[tokio::test]
async fn a_matching_regular_container_status_is_never_confirmation() {
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    // The trap is armed AFTER the birth downsize, so the launcher's own init
    // status genuinely reads 250m while the launcher NAME appears in the regular
    // container statuses carrying exactly the value confirmation is looking for.
    fixture.pod.stop_actuating();
    fixture
        .pod
        .add_misleading_regular_launcher_status(&format!("{ADMITTED_CEILING}m"));

    let result = fixture.lift().await;

    assert!(
        matches!(result, LeaseResult::DegradedUnleased { .. }),
        "a matching regular-container status must never confirm: {result:?}"
    );
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{BIRTH_MILLICORES}m").as_str()),
        "and the launcher really was still at its birth limit — the degrade is \
         not an artefact of a Pod that had already moved"
    );
    assert_eq!(
        fixture.permit_state().await,
        BuildPodPermitState::DropRequired
    );
}

// ── AC4: every named uncertainty degrades, with drop active ────────────────

/// One fault-injection case.
struct Case {
    name: &'static str,
    reason: DegradedUnleasedReason,
    /// Whether a PATCH may be issued at all. The identity fences run before the
    /// PATCH, so their cases must show a counter of zero.
    patches_allowed: bool,
    budget: Duration,
    /// Applied after the permit captured its identity AND after the birth
    /// downsize — the exact moment a real lift starts from. Injecting a fault
    /// earlier would break the birth downsize instead of the lift.
    disturb: fn(&StoredTaskRunPod),
}

/// **ACCEPTANCE CRITERION 4.**
///
/// Every named uncertainty, each as its own case, driven end to end through the
/// real grant path. Each must be a [`LeaseResult::DegradedUnleased`] carrying a
/// settled reason — never `Ok`, never an `Err`, never `LeaseUnavailable` (which
/// the invocation runner treats as retryable and would re-ask until the queue
/// deadline burned) — and each must leave the durable permit in
/// `drop_required` so the drop reconciler returns the Pod to its birth limit.
///
/// NAMED FAILING MUTATION: collapse any single case into the success arm — e.g.
/// make `is_retryable` return `false` for `StatusStale` and have
/// `patch_and_confirm` return `Ok(())` on it — and exactly that row fails.
///
/// NAMED FAILING MUTATION 2: change the `Authorized` arm of `fold_into_grant` to
/// propagate the failure as an `Err`/`LeaseUnavailable` instead of
/// `DegradedUnleased`, and the `assert_ne!` below fails on every row.
#[tokio::test]
async fn every_named_uncertainty_degrades_with_drop_required() {
    let cases = [
        Case {
            name: "HTTP 200, init status stale (the kubelet never actuates)",
            reason: DegradedUnleasedReason::LiftStatusStale,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| pod.stop_actuating(),
        },
        Case {
            name: "HTTP 200, init status carries no cpu limit at all",
            reason: DegradedUnleasedReason::LiftStatusAbsent,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| {
                pod.stop_actuating();
                pod.clear_launcher_status_limit();
            },
        },
        Case {
            name: "a matching REGULAR-container status while init is stale",
            reason: DegradedUnleasedReason::LiftStatusStale,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| {
                pod.stop_actuating();
                pod.add_misleading_regular_launcher_status(&format!("{ADMITTED_CEILING}m"));
            },
        },
        Case {
            name: "a PodResizePending condition is present",
            reason: DegradedUnleasedReason::LiftResizePending,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| pod.add_resize_pending(),
        },
        Case {
            name: "the Pod was recreated under the same NAME with a new UID",
            reason: DegradedUnleasedReason::ResizeIdentityChanged,
            patches_allowed: false,
            budget: NO_WAIT,
            disturb: |pod| pod.recreate_under_same_name("pod-uid-replacement"),
        },
        Case {
            name: "the launcher declares a different authority protocol",
            reason: DegradedUnleasedReason::LauncherProtocolChanged,
            patches_allowed: false,
            budget: NO_WAIT,
            disturb: |pod| pod.set_observed_protocol("leaf-v1"),
        },
        Case {
            name: "the launcher sidecar restarted (new containerID)",
            reason: DegradedUnleasedReason::LauncherRestarted,
            patches_allowed: false,
            budget: NO_WAIT,
            disturb: |pod| pod.restart_launcher("containerd://restarted"),
        },
        Case {
            name: "the confirmation budget was spent and the status never moved",
            reason: DegradedUnleasedReason::LiftDeadlineExceeded,
            patches_allowed: true,
            budget: Duration::from_millis(8),
            disturb: |pod| pod.stop_actuating(),
        },
        Case {
            name: "the PATCH timed out in transport (no HTTP status at all)",
            reason: DegradedUnleasedReason::ResizeSurfaceUnavailable,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| pod.fail_patches(ApiFault::timeout()),
        },
        Case {
            name: "the apiserver answered 403 (no pods/resize RBAC rule)",
            reason: DegradedUnleasedReason::ResizeForbidden,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| pod.fail_patches(ApiFault::forbidden()),
        },
        Case {
            name: "the apiserver answered 422 (the resize itself was rejected)",
            reason: DegradedUnleasedReason::ResizeRejected,
            patches_allowed: true,
            budget: NO_WAIT,
            disturb: |pod| pod.fail_patches(ApiFault::unprocessable()),
        },
        Case {
            name: "no Pod carries the task run's label any more",
            reason: DegradedUnleasedReason::LiftPodAbsent,
            patches_allowed: false,
            budget: NO_WAIT,
            disturb: |pod| {
                pod.uid_fenced_delete(RUN, POD_UID)
                    .expect("the fenced delete matches the stored uid");
            },
        },
    ];

    for case in cases {
        let fixture = Fixture::armed(healthy_pod(), case.budget).await;
        // Every case starts from the SAME healthy, birth-downsized Pod. The
        // fault is installed here, at the exact moment a real lift begins, so a
        // case cannot pass by having broken the birth downsize instead.
        (case.disturb)(&fixture.pod);

        let result = fixture.lift().await;

        assert_eq!(
            result,
            LeaseResult::DegradedUnleased {
                reason: case.reason
            },
            "case `{}` must degrade with its own settled reason",
            case.name
        );
        assert_ne!(
            result,
            LeaseResult::LeaseUnavailable,
            "case `{}`: LeaseUnavailable means ASK AGAIN, and this answer cannot \
             change by asking",
            case.name
        );
        assert_eq!(
            fixture.permit_state().await,
            BuildPodPermitState::DropRequired,
            "case `{}` must leave the permit drop-required so the reconciler \
             returns the Pod to its birth limit",
            case.name
        );
        if !case.patches_allowed {
            assert_eq!(
                fixture.pod.resize_patches(),
                0,
                "case `{}` is an identity fence: it must land BEFORE any PATCH",
                case.name
            );
        }
    }
}

/// The set of reasons the table produces is not one catch-all wearing eleven
/// hats. Asserted structurally so collapsing two cases onto a shared reason
/// fails here rather than quietly.
///
/// The one deliberate pair is `LiftStatusStale`: the misleading
/// regular-container case and the plain stale case share it **because the
/// launcher's own init status is stale in both**, and telling them apart would
/// require reading `status.containerStatuses` — the exact thing this stack is
/// forbidden to do. Encoded here so the overlap is a stated decision rather than
/// an accident.
#[test]
fn the_apply_time_reasons_are_a_closed_distinct_set() {
    use DegradedUnleasedReason as R;
    let reasons = [
        R::LiftStatusStale,
        R::LiftStatusAbsent,
        R::LiftResizePending,
        R::ResizeIdentityChanged,
        R::LauncherProtocolChanged,
        R::LauncherRestarted,
        R::LiftDeadlineExceeded,
        R::ResizeSurfaceUnavailable,
        R::ResizeForbidden,
        R::ResizeRejected,
        R::LiftPodAbsent,
    ];
    for (i, a) in reasons.iter().enumerate() {
        for b in &reasons[i + 1..] {
            assert_ne!(a, b, "each apply-time outcome needs its own reason");
        }
    }
}

// ── AC5: the UID fence is the caller's, and it exists ──────────────────────

/// **ACCEPTANCE CRITERION 5.**
///
/// `PodResizeClient::resize_launcher_cpu(pod_name, target)` takes only a name
/// and never reads `metadata.uid`. A Pod deleted and recreated under the same
/// name is therefore, to it, the same Pod — and it would patch the replacement.
/// The fence is the caller's, and this proves the caller has it: the refusal
/// lands with the PATCH counter at **zero**.
///
/// NAMED FAILING MUTATION: delete the `observed.pod_uid != intent.pod_uid`
/// comparison in `ResizeLift::observe_and_fence` and this test fails with PATCH
/// count 1 (the replacement Pod is healthy and actuates, so it would confirm and
/// grant).
///
/// NON-VACUITY: the companion assertion below proves the same counter is
/// provably non-zero when the UID matches, so a lift that simply never patches
/// cannot pass this.
#[tokio::test]
async fn a_pod_recreated_under_the_same_name_is_refused_before_any_patch() {
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    let name_before = fixture
        .pod
        .observe_launcher()
        .unwrap()
        .unwrap()
        .pod_name
        .clone();

    fixture.pod.recreate_under_same_name("pod-uid-replacement");

    let after = fixture.pod.observe_launcher().unwrap().unwrap();
    assert_eq!(
        after.pod_name, name_before,
        "the replacement must reuse the NAME — that is what makes the name \
         insufficient as an identity"
    );
    assert_ne!(after.pod_uid, POD_UID, "and carry a different UID");

    let result = fixture.lift().await;

    assert_eq!(
        result,
        LeaseResult::DegradedUnleased {
            reason: DegradedUnleasedReason::ResizeIdentityChanged
        }
    );
    assert_eq!(
        fixture.pod.resize_patches(),
        0,
        "ZERO PATCHes: the UID fence must land before the object is touched"
    );
}

/// The non-vacuity half of the criterion above, stated as its own test so it
/// cannot be deleted with the assertion it protects.
#[tokio::test]
async fn the_matching_uid_happy_path_patches_at_least_once() {
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    assert_eq!(fixture.pod.resize_patches(), 0, "precondition");
    let result = fixture.lift().await;
    assert!(matches!(result, LeaseResult::Status(_)), "{result:?}");
    assert!(
        fixture.pod.resize_patches() >= 1,
        "the same counter the fence test reads as zero must be non-zero here, or \
         that test proves only that nothing ever patches"
    );
}

// ── AC8: body shape and neutrality across cycles ───────────────────────────

/// **ACCEPTANCE CRITERION 8, the serialization half.**
///
/// The emitted body is exactly `spec.initContainers[0].name` and
/// `.resources.limits.cpu` — no `requests`, no `spec.containers`, no second
/// field, no second init container, and no caller-supplied target (the target is
/// derived, and `LeaseGrantRequest` has nowhere to put one — see
/// `resize_authorization_tests`).
#[test]
fn the_patch_body_carries_exactly_one_mutable_field() {
    let body = djinn_k8s::pod_resize::build_resize_patch(
        djinn_k8s::pod_resize::CpuLimit::from_millis(ADMITTED_CEILING),
    );
    let spec = body.get("spec").expect("a spec");
    assert_eq!(
        spec.as_object().map(serde_json::Map::len),
        Some(1),
        "spec has exactly one key"
    );
    assert!(spec.get("containers").is_none(), "never `spec.containers`");
    let containers = spec
        .get("initContainers")
        .and_then(serde_json::Value::as_array)
        .expect("initContainers");
    assert_eq!(containers.len(), 1, "exactly one init container is named");
    let entry = containers[0].as_object().expect("an object");
    assert_eq!(entry.len(), 2, "exactly `name` and `resources`");
    let resources = entry["resources"].as_object().expect("resources");
    assert_eq!(resources.len(), 1, "exactly `limits`");
    assert!(
        resources.get("requests").is_none(),
        "a limits-only resize must never move `requests`: that is scheduling and \
         Kueue accounting"
    );
    let limits = resources["limits"].as_object().expect("limits");
    assert_eq!(limits.len(), 1, "exactly `cpu`");
    assert_eq!(limits["cpu"], format!("{ADMITTED_CEILING}m"));
}

/// **ACCEPTANCE CRITERION 8, the neutrality half — cluster-free.**
///
/// Twenty lift cycles on one Pod. `resources.requests`, the QoS class, the
/// launcher's container ID and its restart count must be byte-identical
/// throughout, and the second init container standing beside the launcher must
/// survive every one.
///
/// NAMED FAILING MUTATION: swap `Patch::Strategic` for `Patch::Merge` in
/// `KubePodResizeApi::patch_resize` — an RFC 7386 merge replaces the whole
/// `initContainers` array, so `init_container_count()` drops from 2 to 1 and this
/// test fails. (The fixture applies strategic-merge semantics for
/// `initContainers` explicitly, so the assertion is about the semantics the
/// production client selects.)
///
/// LIVE-CLUSTER GAP, stated rather than papered over: the criterion asks for
/// these twenty cycles on a kind apiserver. This run is cluster-free — it drives
/// the production observation, patch-body and confirmation code, but not a real
/// kubelet.
#[tokio::test]
async fn twenty_lift_cycles_leave_requests_qos_and_the_container_untouched() {
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    let pod = &fixture.pod;

    let requests = pod.launcher_spec_cpu_request();
    let qos = pod.qos_class();
    let container_id = pod.launcher_container_id();
    let restarts = pod.launcher_restart_count();
    let init_containers = pod.init_container_count();
    assert_eq!(
        init_containers, 2,
        "a second init container must stand beside the launcher"
    );
    assert!(requests.is_some() && qos.is_some() && container_id.is_some());

    // The lifecycle only admits one lift per permit, so the cycling is done at
    // the surface: the same limits-only resize, applied over and over, exactly
    // as a lift/drop pair would.
    for cycle in 0..20 {
        for target in [250, ADMITTED_CEILING] {
            pod.resize_launcher_cpu(&format!("taskrun-{POD_UID}"), target)
                .await
                .unwrap_or_else(|e| panic!("cycle {cycle} target {target}m: {e}"));
        }
        assert_eq!(
            pod.launcher_spec_cpu_request(),
            requests,
            "cycle {cycle}: requests moved"
        );
        assert_eq!(pod.qos_class(), qos, "cycle {cycle}: QoS class moved");
        assert_eq!(
            pod.launcher_container_id(),
            container_id,
            "cycle {cycle}: the launcher was replaced"
        );
        assert_eq!(
            pod.launcher_restart_count(),
            restarts,
            "cycle {cycle}: the launcher restarted"
        );
        assert_eq!(
            pod.init_container_count(),
            init_containers,
            "cycle {cycle}: an init container was dropped — this is what a Merge \
             patch does to an array with patchMergeKey `name`"
        );
    }
    assert!(
        pod.resize_patches() >= 40,
        "the cycles must actually have patched"
    );
}

// ── AC6 / AC7: the admitted ceiling is the effective bound, and it comes from
//              the write-once row ──────────────────────────────────────────

/// **ACCEPTANCE CRITERION 6, cluster-free half.**
///
/// The deployment default is 4000m; this Pod was admitted at 2500m. The value
/// that ends up in `status.initContainerStatuses` must be that Pod's OWN
/// ceiling, and **zero PATCH bodies** may carry a value above it — measured on
/// the bodies actually sent, parsed back through `CpuLimit` so the apiserver's
/// canonicalisation cannot disguise an above-ceiling request.
///
/// NAMED FAILING MUTATION: substitute the deployment default for
/// `identity.admitted_cpu_millicores` in `resize_authorization::resolve_target`
/// and the above-ceiling body counter becomes 1 (a 4000m body against a 2500m
/// ceiling). The clamp that would otherwise catch such a body has moved to
/// `resize_lift::clamp_to_admitted_ceiling`, whose own mutation is
/// `an_intent_above_its_own_ceiling_still_patches_at_the_ceiling`.
///
/// LIVE-CLUSTER GAP, stated rather than papered over: the criterion also asks
/// for a companion control that issues the same oversubscribed value DIRECTLY
/// through `pods/resize` against a live apiserver, to prove the apiserver does
/// not bound this and the server clamp is the only effective bound. That half
/// needs a kind cluster and is NOT done here.
#[tokio::test]
async fn the_clamp_is_the_effective_bound_measured_on_the_patch_bodies() {
    const {
        assert!(
            DEPLOYMENT_DEFAULT_LEASED > ADMITTED_CEILING,
            "the deployment default must exceed the admitted ceiling, or this \
             test cannot distinguish the two sources"
        );
    }
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    let result = fixture.lift().await;
    assert!(matches!(result, LeaseResult::Status(_)), "{result:?}");

    let bodies = fixture.pod.patched_cpu_millicores();
    assert!(!bodies.is_empty(), "the lift must actually have patched");
    assert_eq!(
        bodies
            .iter()
            .filter(|millis| **millis > ADMITTED_CEILING)
            .count(),
        0,
        "ZERO PATCH bodies above the admitted ceiling; observed {bodies:?}"
    );
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{ADMITTED_CEILING}m").as_str()),
        "and the value the launcher ends up holding is the CEILING, not the \
         configured 4000m"
    );
}

/// **ACCEPTANCE CRITERION 7, both halves — and `gvix`'s criterion 1.**
///
/// A Pod rendered with a per-project `build_resources.task.cpu_limit` override
/// is admitted with a launcher ceiling ABOVE the deployment default. Two things
/// must hold, and `0ppk-1a` only delivered the first:
///
/// * **PROVENANCE.** The bound must be that Pod's own captured value, not the
///   process's `launcher_leased_millicores`.
/// * **BEHAVIOUR.** The launcher must actually END UP at 8000m. The override
///   raised the lease the Pod's own launcher hands out; a lift that stopped at
///   the 4000m default would strand half the CPU the project configured —
///   7deu's ancestor clamp, re-entered through the override door.
///
/// The second half is asserted on `status.initContainerStatuses` — the only
/// field that confirms a resize — and on the PATCH bodies actually sent, in
/// MILLICORES. String comparison would not do: `#2861` observed the apiserver
/// canonicalise `2000m` to `2`.
///
/// NAMED FAILING MUTATIONS, either of which must break this test:
/// * Substitute the deployment default for `identity.admitted_cpu_millicores`
///   in `resize_authorization::resolve_target` — the carried bound reads 4000.
/// * Restore `0ppk-1a`'s clamp, `ceiling.min(deployment_default)` — the target,
///   the PATCH bodies and the confirmed status all read 4000 against an 8000m
///   ceiling.
#[tokio::test]
async fn an_override_pod_lifts_to_its_own_admitted_ceiling() {
    const OVERRIDE_CEILING: u64 = 8_000;
    const {
        assert!(
            OVERRIDE_CEILING > DEPLOYMENT_DEFAULT_LEASED,
            "the per-project override must RAISE the Pod's ceiling above the \
             deployment default, or there is no provenance to distinguish"
        );
    }
    let fixture = Fixture::armed(pod_admitted_at(OVERRIDE_CEILING), NO_WAIT).await;
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{BIRTH_MILLICORES}m").as_str()),
        "precondition: the launcher sits at its birth limit, so reaching 8000m \
         is something the lift had to DO"
    );

    let result = fixture.lift().await;
    assert!(matches!(result, LeaseResult::Status(_)), "{result:?}");

    let intents = fixture.intents();
    assert_eq!(intents.len(), 1, "exactly one intent reached the boundary");
    assert_eq!(
        intents[0].admitted_cpu_millicores,
        i64::try_from(OVERRIDE_CEILING).unwrap(),
        "the bound must be the Pod's OWN captured ceiling. Reading 4000 here \
         means the ceiling was recomputed from the deployment default, which \
         clamps every per-project-override Pod below its own rendered lease"
    );
    assert_eq!(
        intents[0].target_millicores,
        i64::try_from(OVERRIDE_CEILING).unwrap(),
        "AND THE TARGET IS THAT CEILING. Reading 4000 here is `0ppk-1a`'s \
         min() against the deployment default coming back — the AC6/AC7 \
         contradiction `gvix` exists to resolve"
    );
    assert_eq!(
        fixture.pod.patched_cpu_millicores(),
        vec![OVERRIDE_CEILING],
        "the PATCH body sent to the apiserver carries the override, in \
         millicores"
    );
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{OVERRIDE_CEILING}m").as_str()),
        "and the launcher's INIT-container status — the only field that \
         confirms a resize — must have MOVED from {BIRTH_MILLICORES}m to \
         {OVERRIDE_CEILING}m, not to the 4000m default"
    );
    assert_eq!(
        fixture.permit_state().await,
        BuildPodPermitState::Lifted,
        "a confirmed lift advances the durable lifecycle to `lifted`"
    );
}

/// **`0ppk-1a` ACCEPTANCE CRITERION 2, relocated to the site that sends the
/// PATCH.**
///
/// `gvix` removed the deployment default from the target derivation, so
/// `resize_authorization` can no longer produce an over-ceiling target at all.
/// That must not mean the over-ceiling protection stopped being tested — so it
/// is exercised where it now lives: an intent is built BY HAND carrying a
/// target above its own `admitted_cpu_millicores` and handed straight to the
/// real [`ResizeLift`]. [`PodResizeIntent`] is a `pub` struct of `pub` fields,
/// so this is not a hypothetical caller.
///
/// Zero PATCH bodies above the ceiling, measured on the bodies actually sent
/// and parsed back through `CpuLimit`.
///
/// NAMED FAILING MUTATION: replace the `.min(...)` in
/// `resize_lift::clamp_to_admitted_ceiling` with `intent.target_millicores`,
/// and the above-ceiling body counter becomes 1 — a 9999m body against a 2500m
/// ceiling.
#[tokio::test]
async fn an_intent_above_its_own_ceiling_still_patches_at_the_ceiling() {
    const OVER_CEILING: i64 = 9_999;
    const {
        assert!(
            OVER_CEILING as u64 > ADMITTED_CEILING,
            "the hand-built target must exceed the ceiling, or the clamp has \
             nothing to bite on"
        );
    }
    let fixture = Fixture::armed(healthy_pod(), NO_WAIT).await;
    let permit = fixture
        .permits
        .active(RUN)
        .await
        .expect("the permit is readable")
        .expect("the permit exists");
    let identity = permit
        .resize_identity
        .clone()
        .expect("the fixture captured a resize identity");
    assert_eq!(
        permit.state,
        BuildPodPermitState::BirthConfirmed,
        "precondition: the lift starts from birth_confirmed"
    );

    // Every coordinate is the durable permit's; only the target is forged.
    let intent = crate::resize_authorization::PodResizeIntent {
        task_run_id: RUN.into(),
        invocation_id: INVOCATION.into(),
        permit_id: permit.permit_id.clone(),
        fencing_token: permit.fencing_token,
        permit_state: permit.state,
        pod_namespace: identity.pod_namespace.clone(),
        pod_name: identity.pod_name.clone(),
        pod_uid: identity.pod_uid.clone(),
        launcher_container_name: identity.launcher_container_name.clone(),
        launcher_container_id: identity.launcher_container_id.clone(),
        effective_launcher_protocol: identity
            .effective_launcher_protocol
            .parse()
            .expect("the fixture renders a known protocol"),
        target_millicores: OVER_CEILING,
        admitted_cpu_millicores: identity.admitted_cpu_millicores,
    };

    let applied = fixture.applier.inner.apply(&intent).await;
    assert!(
        applied.is_ok(),
        "the clamped lift still confirms: {applied:?}"
    );

    let bodies = fixture.pod.patched_cpu_millicores();
    assert!(!bodies.is_empty(), "the lift must actually have patched");
    assert_eq!(
        bodies
            .iter()
            .filter(|millis| **millis > ADMITTED_CEILING)
            .count(),
        0,
        "ZERO PATCH bodies above the admitted ceiling; observed {bodies:?}"
    );
    assert_eq!(
        fixture.pod.launcher_status_cpu().as_deref(),
        Some(format!("{ADMITTED_CEILING}m").as_str()),
        "and the launcher ends up holding the CEILING, not the forged 9999m"
    );
}
