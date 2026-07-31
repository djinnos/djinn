//! The server-derived resize authorization, proved against a real Postgres.
//!
//! There is no `Fake` or `Mock` repository here for anything under test. The
//! permits, the lease FIFO and the durable invocation-lease authority are the
//! production types over an ephemeral Postgres database, because every property
//! asserted below is a property of durable rows: who owns an invocation, which
//! Pod a permit was captured against, and what ceiling was admitted. A double
//! whose `active()` returned a struct literal would agree with all of it and
//! prove none of it.
//!
//! The ONE double is [`CountingPodResizeIntentSink`], and it is deliberately on
//! the far side of the boundary: it stands in for the Kubernetes `pods/resize`
//! PATCH that `0ppk-1b` owns. "Zero Kubernetes calls" is only assertable if
//! something counts calls, and a returned `DegradedUnleased` cannot tell a
//! refusal that emitted no intent apart from one that emitted an intent and then
//! reported a degrade anyway.

use std::str::FromStr;
use std::sync::Arc;

use djinn_db::{
    AcquireBuildPodPermitResult, BuildLeaseRepository, BuildPodPermitRepository,
    BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, Database,
    InvocationLeaseAuthorityRepository, InvocationLeaseMode,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_supervisor::services::{
    DegradedUnleasedReason, DurableInvocationLiftAuthority, InvocationLiftAuthority,
    InvocationLiftDecision, LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest, LeaseIdentity,
    LeaseQueueRequest, LeaseResult, TaskInvocationLeaseIdentity,
};

use crate::build_lease::BuildLeaseService;
use crate::resize_authorization::{
    CountingPodResizeIntentSink, RefusalClass, ResizeAuthority, ResizeAuthorizationOutcome,
};

/// `DJINN_MAX_BUILD_TASKRUNS` stand-in — large enough that capacity never
/// participates in an authorization assertion.
const CONFIGURED_CAP: i64 = 9;

/// `DJINN_LAUNCHER_LEASED_MILLICORES` as the default render sets it.
const CONFIGURED_LEASED: i64 = 4_000;

/// The ceiling `g8jk-3` captured from the STORED Pod. Deliberately BELOW
/// [`CONFIGURED_LEASED`] — a mutating webhook shrank what was rendered — so the
/// clamp has something to bite on and an unclamped target is a different number
/// rather than a coincidence.
const ADMITTED_CEILING: i64 = 2_500;

const OWNER_RUN: &str = "01983f00-0000-7000-8000-00000000a001";
const OWNER_TASK: &str = "01983f00-0000-7000-8000-00000000b001";
const OWNER_INVOCATION: &str = "01983f00-0000-7000-8000-00000000c001";

const INTRUDER_RUN: &str = "01983f00-0000-7000-8000-00000000a002";
const INTRUDER_TASK: &str = "01983f00-0000-7000-8000-00000000b002";

fn identity(task: &str, run: &str, invocation: &str) -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: task.into(),
        task_run_id: run.into(),
        invocation_id: invocation.into(),
    })
}

/// The permit identity a task run's Pod is captured with. Every coordinate is
/// distinct per task run so an intent built from the WRONG permit is visibly the
/// wrong Pod rather than an equal-looking one.
fn resize_identity(
    run: &str,
    ceiling: i64,
    protocol: LauncherAuthorityProtocol,
) -> BuildPodResizeIdentity {
    BuildPodResizeIdentity {
        pod_namespace: format!("ns-{run}"),
        pod_name: format!("pod-{run}"),
        pod_uid: format!("uid-{run}"),
        launcher_container_name: format!("cgroup-launcher-{run}"),
        launcher_container_id: format!("containerd://{run}"),
        image_digest: format!("sha256:{run}"),
        observed_launcher_protocol: protocol.as_wire().into(),
        effective_launcher_protocol: protocol.as_wire().into(),
        admitted_cpu_millicores: ceiling,
    }
}

/// `build_pod_permits.task_run_id` is a real foreign key onto `task_runs`, so a
/// permit cannot be acquired for a task run that does not exist. Seeded through
/// the repositories rather than with hand-written SQL — `djinn-coordinator` is
/// outside the raw-SQL boundary, and the FK chain (user → project → task → run)
/// is exactly what the guard exists to keep out of this crate.
async fn seed_task_runs(db: &Database, runs: &[&str]) {
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
    let repository = djinn_db::TaskRunRepository::new(db.clone());
    for run in runs {
        repository
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
}

/// A live coordinator composition: the real lease FIFO, the real permit
/// relation, the real durable lift authority, and the resize authorization
/// installed on the grant path.
struct Fixture {
    service: Arc<BuildLeaseService>,
    permits: Arc<BuildPodPermitRepository>,
    sink: Arc<CountingPodResizeIntentSink>,
    authority: Arc<ResizeAuthority>,
    _db: Database,
}

impl Fixture {
    async fn armed() -> Self {
        Self::with_mode(InvocationLeaseMode::Enforce).await
    }

    async fn with_mode(mode: InvocationLeaseMode) -> Self {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        seed_task_runs(&db, &[OWNER_RUN, INTRUDER_RUN]).await;

        let lease_authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
        let seeded = lease_authority.seed_baseline().await.unwrap();
        lease_authority
            .set_mode_and_cap(seeded.epoch, mode, Some(CONFIGURED_CAP))
            .await
            .unwrap();

        let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
        let permits = Arc::new(BuildPodPermitRepository::new(db.clone()));
        let sink = Arc::new(CountingPodResizeIntentSink::new());
        let lift: Arc<dyn InvocationLiftAuthority> = Arc::new(DurableInvocationLiftAuthority::new(
            db.clone(),
            "resize-authorization-tests",
        ));
        let authority = Arc::new(ResizeAuthority::new(
            Arc::clone(&leases),
            Arc::clone(&permits),
            lift,
            CONFIGURED_LEASED,
            Arc::clone(&sink) as Arc<dyn crate::resize_authorization::PodResizeIntentSink>,
        ));

        let service = Arc::new(
            BuildLeaseService::new(Arc::clone(&leases), CONFIGURED_CAP)
                .with_invocation_lease_authority(Arc::clone(&lease_authority))
                .with_resize_authority(Arc::clone(&authority)),
        );
        assert!(matches!(service.recover().await, LeaseResult::Status(_)));

        Self {
            service,
            permits,
            sink,
            authority,
            _db: db,
        }
    }

    /// Create the permit row and capture its write-once resize identity — the
    /// two writes `0ppk-1b` will make from the live dispatch path, and which
    /// nothing on `main` makes yet. Seeded directly here on purpose; see the
    /// module header of `resize_authorization`.
    async fn seed_permit(&self, run: &str, ceiling: i64, protocol: LauncherAuthorityProtocol) {
        let AcquireBuildPodPermitResult::Acquired { row, .. } =
            self.permits.acquire(run, CONFIGURED_CAP).await
        else {
            panic!("the permit pool must admit {run}");
        };
        // `capture_resize_identity` only fires from `job_created`, which is the
        // state a Job-backed dispatch reaches before its Pod has a UID.
        let bound = self
            .permits
            .bind_or_refresh_job_uid(
                run,
                &row.permit_id,
                row.fencing_token,
                &format!("job-{run}"),
            )
            .await;
        assert!(
            matches!(bound, Ok(djinn_db::BindBuildPodPermitResult::Bound(_))),
            "binding the Job UID must succeed: {bound:?}"
        );
        let captured = self
            .permits
            .capture_resize_identity(
                run,
                &row.permit_id,
                row.fencing_token,
                &resize_identity(run, ceiling, protocol),
            )
            .await
            .unwrap();
        assert!(
            matches!(captured, CaptureBuildPodResizeIdentityResult::Captured(_)),
            "the write-once resize identity must capture: {captured:?}"
        );
    }

    /// Escalate an invocation onto the FIFO and return the fencing token the
    /// durable row was minted with.
    async fn granted(&self, task: &str, run: &str, invocation: &str) -> LeaseFencingToken {
        match self
            .service
            .queue(LeaseQueueRequest {
                identity: identity(task, run, invocation),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await
        {
            LeaseResult::Granted(grant) => grant.fencing_token,
            other => panic!("expected a grant for {invocation}, got {other:?}"),
        }
    }

    /// Acknowledge the grant — the call that authorizes the resize.
    async fn grant(&self, id: LeaseIdentity, token: &LeaseFencingToken) -> LeaseResult {
        self.service
            .grant(LeaseGrantRequest {
                identity: id,
                fencing_token: token.clone(),
            })
            .await
    }
}

// ─── AC1: no coordinates, and a foreign invocation is denied ────────────────

/// **ACCEPTANCE CRITERION 1, the structural half.**
///
/// The worker sends no coordinates because there is nowhere to put one. This
/// destructuring is exhaustive: adding `pod_name`, `container_index` or
/// `target_millicores` to [`LeaseGrantRequest`] makes this file stop compiling,
/// which is a stronger statement than any runtime assertion could make about a
/// field that does not exist.
#[test]
fn a_grant_request_carries_no_pod_coordinates_and_no_cpu_target() {
    let LeaseGrantRequest {
        identity: _,
        fencing_token: _,
    } = LeaseGrantRequest {
        identity: identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION),
        fencing_token: LeaseFencingToken(1),
    };
}

/// **ACCEPTANCE CRITERION 1, the behavioural half.**
///
/// Two task runs, two permits, two Pods. The intruder holds a live grant of its
/// own and names the OWNER's invocation. It must be refused, and — the part that
/// matters — the Kubernetes boundary must not be reached at all. Asserted on the
/// counter, not on the returned value: a denial that had already emitted a PATCH
/// intent would return exactly the same `DegradedUnleased`.
///
/// NON-VACUITY (run, do not assume): make `ResizeAuthority::authorize` accept a
/// caller-supplied target — short-circuiting derivation and emitting the intent
/// straight from the request — and `calls()` here becomes non-zero.
#[tokio::test]
async fn a_task_run_naming_another_invocation_is_denied_with_zero_kubernetes_calls() {
    let fixture = Fixture::armed().await;
    fixture
        .seed_permit(
            OWNER_RUN,
            ADMITTED_CEILING,
            LauncherAuthorityProtocol::ResizeV2,
        )
        .await;
    fixture
        .seed_permit(INTRUDER_RUN, 16_000, LauncherAuthorityProtocol::ResizeV2)
        .await;

    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    assert_eq!(
        fixture.sink.calls(),
        0,
        "precondition: queueing must not reach the Kubernetes boundary"
    );

    // The intruder presents the owner's invocation id and fencing token under
    // its OWN task and run. Everything it controls is in this request.
    let refused = fixture
        .grant(
            identity(INTRUDER_TASK, INTRUDER_RUN, OWNER_INVOCATION),
            &token,
        )
        .await;
    assert_eq!(
        refused,
        LeaseResult::DegradedUnleased {
            reason: DegradedUnleasedReason::NotTheInvocationOwner
        },
        "a task run naming another task run's invocation must be denied"
    );
    assert_eq!(
        fixture.sink.calls(),
        0,
        "ZERO Kubernetes calls: the denial must land before any Pod is named"
    );
    assert!(
        fixture.sink.intents().is_empty(),
        "and no intent may have been derived for the intruder's Pod either"
    );
}

/// The intruder cannot reach the owner's Pod, and the owner still can. Without
/// this the previous test would pass on an authority that refuses everything.
///
/// It is also where "derived SERVER-SIDE" is proved: the intent's namespace, Pod
/// name, Pod UID and container name are the OWNER's durable permit values, and
/// the intruder's differently-named permit — live in the same database, with a
/// 16000m ceiling — is never selected.
#[tokio::test]
async fn the_owner_is_authorized_against_its_own_durable_permit() {
    let fixture = Fixture::armed().await;
    fixture
        .seed_permit(
            OWNER_RUN,
            ADMITTED_CEILING,
            LauncherAuthorityProtocol::ResizeV2,
        )
        .await;
    fixture
        .seed_permit(INTRUDER_RUN, 16_000, LauncherAuthorityProtocol::ResizeV2)
        .await;

    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    let granted = fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;
    assert!(
        matches!(granted, LeaseResult::Status(_)),
        "an authorized grant returns the lease status unchanged: {granted:?}"
    );

    let intents = fixture.sink.intents();
    assert_eq!(intents.len(), 1, "exactly one Kubernetes intent");
    let intent = &intents[0];
    assert_eq!(intent.pod_namespace, format!("ns-{OWNER_RUN}"));
    assert_eq!(intent.pod_name, format!("pod-{OWNER_RUN}"));
    assert_eq!(intent.pod_uid, format!("uid-{OWNER_RUN}"));
    assert_eq!(
        intent.launcher_container_name,
        format!("cgroup-launcher-{OWNER_RUN}")
    );
    assert_eq!(
        intent.admitted_cpu_millicores, ADMITTED_CEILING,
        "the ceiling must come from the OWNER's permit, not the intruder's 16000m"
    );
}

/// A stale or guessed fencing token is a denial, and it too costs zero calls.
#[tokio::test]
async fn a_mismatched_fencing_token_is_denied_with_zero_kubernetes_calls() {
    let fixture = Fixture::armed().await;
    fixture
        .seed_permit(
            OWNER_RUN,
            ADMITTED_CEILING,
            LauncherAuthorityProtocol::ResizeV2,
        )
        .await;
    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;

    let refused = fixture
        .authority
        .authorize(
            &TaskInvocationLeaseIdentity {
                task_id: OWNER_TASK.into(),
                task_run_id: OWNER_RUN.into(),
                invocation_id: OWNER_INVOCATION.into(),
            },
            &LeaseFencingToken(token.0 + 1),
        )
        .await;
    match refused {
        ResizeAuthorizationOutcome::Refused(refusal) => {
            assert_eq!(refusal.class, RefusalClass::Denied);
            assert_eq!(refusal.reason, DegradedUnleasedReason::FencingTokenMismatch);
        }
        other => panic!("a foreign fencing token must be denied, got {other:?}"),
    }
    assert_eq!(fixture.sink.calls(), 0, "ZERO Kubernetes calls");
}

// ─── AC2: the ceiling clamp ─────────────────────────────────────────────────

/// **ACCEPTANCE CRITERION 2.**
///
/// The process is configured to lift to 4000m; the Pod was admitted at 2500m.
/// The target must be the stored ceiling, and NO intent may ask for more than
/// the ceiling it was clamped against.
///
/// NON-VACUITY (run, do not assume): delete `.min(ceiling)` in
/// `resize_authorization::clamp` and `intents_above_ceiling()` becomes 1.
#[tokio::test]
async fn a_configured_lift_above_the_stored_ceiling_clamps_to_the_ceiling() {
    let fixture = Fixture::armed().await;
    fixture
        .seed_permit(
            OWNER_RUN,
            ADMITTED_CEILING,
            LauncherAuthorityProtocol::ResizeV2,
        )
        .await;
    assert!(
        CONFIGURED_LEASED > ADMITTED_CEILING,
        "precondition: the configured lift must exceed the stored ceiling, or \
         this test cannot distinguish a clamp from a passthrough"
    );

    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;

    let intents = fixture.sink.intents();
    assert_eq!(intents.len(), 1);
    assert_eq!(
        intents[0].target_millicores, ADMITTED_CEILING,
        "the target must be min(configured 4000m, admitted 2500m)"
    );
    assert_eq!(
        fixture.sink.intents_above_ceiling(),
        0,
        "ZERO PATCH intents above the stored ceiling"
    );
}

/// The clamp is a `min`, not a constant: a ceiling ABOVE the configured lift
/// must leave the configured value alone. Without this, hard-coding
/// `target = ceiling` would pass the test above.
#[tokio::test]
async fn a_ceiling_above_the_configured_lift_does_not_raise_the_target() {
    let fixture = Fixture::armed().await;
    fixture
        .seed_permit(OWNER_RUN, 16_000, LauncherAuthorityProtocol::ResizeV2)
        .await;
    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;

    let intents = fixture.sink.intents();
    assert_eq!(intents.len(), 1);
    assert_eq!(
        intents[0].target_millicores, CONFIGURED_LEASED,
        "a generous ceiling must not lift the process above what it configured"
    );
    assert_eq!(fixture.sink.intents_above_ceiling(), 0);
}

// ─── AC3: the lift predicate is written against the TYPES ───────────────────

/// **ACCEPTANCE CRITERION 3.**
///
/// The should-we-lift-at-all question is [`InvocationLiftDecision`], read
/// through the durable authority, plus the Pod's own
/// [`LauncherAuthorityProtocol`]. Disarming the authority must stop the resize
/// dead — with zero Kubernetes calls — while leaving the lease itself untouched,
/// because `Shadow` and `Unleased` are deliberate states, not degrades.
///
/// The other half of this criterion is
/// `scripts/check-resize-authorization-boundary.sh`, which fails if the module
/// ever reads the retired relation `flc5` will DROP.
#[tokio::test]
async fn only_an_enforcing_authority_authorizes_a_resize() {
    for (mode, expected) in [
        (InvocationLeaseMode::Off, InvocationLiftDecision::Unleased),
        (InvocationLeaseMode::Shadow, InvocationLiftDecision::Shadow),
    ] {
        let fixture = Fixture::with_mode(mode).await;
        fixture
            .seed_permit(
                OWNER_RUN,
                ADMITTED_CEILING,
                LauncherAuthorityProtocol::ResizeV2,
            )
            .await;
        let token = fixture
            .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
            .await;
        let granted = fixture
            .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
            .await;
        assert!(
            matches!(granted, LeaseResult::Status(_)),
            "{mode:?} is not a degrade: the lease result passes through"
        );
        assert_eq!(
            fixture.sink.calls(),
            0,
            "{mode:?} must reach the Kubernetes boundary ZERO times"
        );

        let outcome = fixture
            .authority
            .authorize(
                &TaskInvocationLeaseIdentity {
                    task_id: OWNER_TASK.into(),
                    task_run_id: OWNER_RUN.into(),
                    invocation_id: OWNER_INVOCATION.into(),
                },
                &token,
            )
            .await;
        assert_eq!(
            outcome,
            ResizeAuthorizationOutcome::NotLifting(expected),
            "the lift predicate must be the decision TYPE, in {mode:?}"
        );
    }
}

/// A `leaf-v1` Pod has no launcher `limits.cpu` to move, so it is not a
/// resizable subject. The comparison is against the shared protocol type, which
/// is what keeps it agreeing with migration 164's CHECK constraint.
#[tokio::test]
async fn a_leaf_v1_pod_is_not_resizable_and_costs_zero_kubernetes_calls() {
    let fixture = Fixture::armed().await;
    fixture
        .seed_permit(
            OWNER_RUN,
            ADMITTED_CEILING,
            LauncherAuthorityProtocol::LeafV1,
        )
        .await;
    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    let refused = fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;
    assert_eq!(
        refused,
        LeaseResult::DegradedUnleased {
            reason: DegradedUnleasedReason::ProtocolNotResizable
        }
    );
    assert_eq!(fixture.sink.calls(), 0, "ZERO Kubernetes calls");
    assert_eq!(
        LauncherAuthorityProtocol::from_str("resize-v2").unwrap(),
        LauncherAuthorityProtocol::ResizeV2,
        "the wire spelling this module parses is the shared type's, not a literal"
    );
}

// ─── AC4: every uncertainty is a DegradedUnleased, never an Err ─────────────

/// **ACCEPTANCE CRITERION 4.**
///
/// Each row is an uncertainty the authorization can hit, driven end to end
/// through the real grant path. Every one must surface as
/// [`LeaseResult::DegradedUnleased`] with its own reason — never as
/// `LeaseUnavailable`, which the invocation runner treats as retryable and would
/// re-ask until the queue deadline burned.
///
/// NON-VACUITY (run, do not assume): collapse `DegradedUnleased` into the
/// success arm — return `granted` from the `Refused` branch of
/// `ResizeAuthority::fold_into_grant` — and every assertion below fails.
#[tokio::test]
async fn every_uncertainty_degrades_unleased_rather_than_erroring() {
    // No permit at all: the shape production is in TODAY, since nothing on
    // `main` creates one.
    let fixture = Fixture::armed().await;
    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    let degraded = fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;
    assert_eq!(
        degraded,
        LeaseResult::DegradedUnleased {
            reason: DegradedUnleasedReason::PermitAbsent
        },
        "a missing permit is a settled degrade, not a retryable error"
    );
    assert_ne!(
        degraded,
        LeaseResult::LeaseUnavailable,
        "LeaseUnavailable means ASK AGAIN; this answer cannot change by asking"
    );
    assert_eq!(fixture.sink.calls(), 0, "ZERO Kubernetes calls");

    // A permit exists but its write-once resize identity was never captured —
    // the window between `acquire` and `g8jk-3`'s post-admission capture.
    let fixture = Fixture::armed().await;
    let acquired = fixture.permits.acquire(OWNER_RUN, CONFIGURED_CAP).await;
    assert!(
        matches!(acquired, AcquireBuildPodPermitResult::Acquired { .. }),
        "the permit pool must admit the owner: {acquired:?}"
    );
    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    let degraded = fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;
    assert_eq!(
        degraded,
        LeaseResult::DegradedUnleased {
            reason: DegradedUnleasedReason::ResizeIdentityUnknown
        },
        "an uncaptured Pod identity is a settled degrade"
    );
    assert_eq!(fixture.sink.calls(), 0, "ZERO Kubernetes calls");
}

/// A refusal must not disturb the durable lease. The invocation still holds its
/// slot and its fencing token; it simply runs unleased. Degrading a lease into
/// nonexistence would be a capacity leak dressed as a safety measure.
#[tokio::test]
async fn a_degraded_resize_leaves_the_lease_itself_intact() {
    let fixture = Fixture::armed().await;
    let token = fixture
        .granted(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION)
        .await;
    let degraded = fixture
        .grant(identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION), &token)
        .await;
    assert!(matches!(degraded, LeaseResult::DegradedUnleased { .. }));

    match fixture
        .service
        .status(djinn_supervisor::services::LeaseStatusRequest {
            identity: identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION),
        })
        .await
    {
        LeaseResult::Status(status) => {
            assert_eq!(
                status.fencing_token,
                Some(token),
                "the lease keeps the fence it was granted"
            );
            assert_eq!(
                status.state,
                djinn_supervisor::services::LeaseState::Launching,
                "and the grant still advanced the durable row"
            );
        }
        other => panic!("the lease must still be readable: {other:?}"),
    }
}

/// Without an authority composed — every composition on `main` — the grant path
/// is byte-for-byte what it was. This is the reachability claim the PR makes,
/// asserted rather than described.
#[tokio::test]
async fn an_uncomposed_authority_leaves_the_grant_path_unchanged() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
    let service = BuildLeaseService::new(Arc::clone(&leases), CONFIGURED_CAP);
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));

    let LeaseResult::Granted(grant) = service
        .queue(LeaseQueueRequest {
            identity: identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await
    else {
        panic!("the invocation must be granted");
    };
    let granted = service
        .grant(LeaseGrantRequest {
            identity: identity(OWNER_TASK, OWNER_RUN, OWNER_INVOCATION),
            fencing_token: grant.fencing_token,
        })
        .await;
    assert!(
        matches!(granted, LeaseResult::Status(_)),
        "with no resize authority composed the grant is unchanged: {granted:?}"
    );
}
