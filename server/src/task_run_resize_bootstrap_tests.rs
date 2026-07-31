//! Tests for [`super`], the post-admission ceiling capture and birth downsize.
//!
//! Split out of the module itself only because the two together exceed the
//! repository's file-size guard. `#[path]` keeps them one compilation unit, so
//! these tests still reach the module's private items.

use super::*;
use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, CreateTaskRunParams, Database,
    EffectiveCreatorProvenance, ProjectRepository, TaskRepository, TaskRunRepository,
    UserRepository,
};
use djinn_k8s::pod_resize::NotConfirmed;
use std::sync::Mutex as StdMutex;

const CEILING: u64 = 4000;

// ── Fixtures ───────────────────────────────────────────────────────────

/// What the fake apiserver has stored, and what it was asked to do.
#[derive(Default)]
struct FakeSurfaceState {
    observation: Option<Result<Option<ObservedLauncherSidecar>, LauncherObservationError>>,
    resize_results: Vec<Result<(), PodResizeError>>,
    resizes: Vec<(String, u64)>,
    deletes: Vec<(String, String)>,
}

struct FakeSurface {
    state: StdMutex<FakeSurfaceState>,
}

impl FakeSurface {
    fn new(observation: Result<Option<ObservedLauncherSidecar>, LauncherObservationError>) -> Self {
        Self {
            state: StdMutex::new(FakeSurfaceState {
                observation: Some(observation),
                ..FakeSurfaceState::default()
            }),
        }
    }

    fn observing(observed: ObservedLauncherSidecar) -> Self {
        Self::new(Ok(Some(observed)))
    }

    fn with_resize_results(self, results: Vec<Result<(), PodResizeError>>) -> Self {
        self.state
            .lock()
            .expect("fake surface")
            .resize_results
            .extend(results);
        self
    }

    fn resizes(&self) -> Vec<(String, u64)> {
        self.state.lock().expect("fake surface").resizes.clone()
    }

    fn deletes(&self) -> Vec<(String, String)> {
        self.state.lock().expect("fake surface").deletes.clone()
    }
}

#[async_trait]
impl TaskRunPodSurface for FakeSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        self.state
            .lock()
            .expect("fake surface")
            .observation
            .clone()
            .expect("observation configured")
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
        let mut state = self.state.lock().expect("fake surface");
        state.resizes.push((pod_name.to_owned(), target_millicores));
        if state.resize_results.is_empty() {
            Ok(())
        } else {
            state.resize_results.remove(0)
        }
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.state
            .lock()
            .expect("fake surface")
            .deletes
            .push((task_run_id.to_owned(), pod_uid.to_owned()));
        Ok(())
    }
}

/// The value the Job manifest was RENDERED with. Every ceiling assertion in
/// this module compares against the *stored* value instead; a test that
/// substituted this constant would be asserting the render input, which is
/// exactly the assertion that proves nothing.
const RENDERED_CEILING: u64 = 4000;

fn observed(
    pod_uid: &str,
    ceiling: Option<u64>,
    protocol: Option<&str>,
) -> ObservedLauncherSidecar {
    ObservedLauncherSidecar {
        namespace: "djinn".to_owned(),
        pod_name: format!("taskrun-{pod_uid}"),
        pod_uid: pod_uid.to_owned(),
        launcher_container_name: "cgroup-launcher".to_owned(),
        launcher_container_id: Some(format!("containerd://{pod_uid}")),
        image_digest: Some("registry/img@sha256:feed".to_owned()),
        observed_protocol: protocol.map(ToOwned::to_owned),
        admitted_cpu_millicores: ceiling,
    }
}

fn resize_v2(pod_uid: &str, ceiling: u64) -> ObservedLauncherSidecar {
    observed(pod_uid, Some(ceiling), Some("resize-v2"))
}

async fn seed_task_run(db: &Database, suffix: &str) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            i64::try_from(uuid::Uuid::now_v7().as_u128() % 8_000_000_000_000_000_000)
                .expect("github id"),
            &format!("resize-bootstrap-{suffix}-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed user");
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(
            &format!("resize-bootstrap-{suffix}"),
            "djinnos",
            &format!("resize-bootstrap-{suffix}"),
        )
        .await
        .expect("seed project");
    let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            None,
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "resize bootstrap",
            "description",
            "design",
            "task",
            2,
            "owner",
            None,
            None,
        )
        .await
        .expect("seed task");
    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed task run");
    task_run_id
}

/// A permit that has reached `job_created` — the only state from which
/// `capture_resize_identity` can succeed.
async fn bound_permit(
    permits: &BuildPodPermitRepository,
    task_run_id: &str,
    job_uid: &str,
) -> PermitBinding {
    let row = match permits.acquire(task_run_id, 16).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("unexpected acquire outcome: {other:?}"),
    };
    match permits
        .bind_or_refresh_job_uid(task_run_id, &row.permit_id, row.fencing_token, job_uid)
        .await
        .expect("bind job uid")
    {
        BindBuildPodPermitResult::Bound(row) => PermitBinding {
            task_run_id: task_run_id.to_owned(),
            permit_id: row.permit_id,
            fencing_token: row.fencing_token,
        },
        other => panic!("unexpected bind outcome: {other:?}"),
    }
}

fn bootstrap_with(
    db: &Database,
    surface: Arc<dyn TaskRunPodSurface>,
) -> (TaskRunResizeBootstrap, Arc<DispatchGate>) {
    let gate = Arc::new(DispatchGate::new());
    (
        TaskRunResizeBootstrap::new(
            BuildPodPermitRepository::new(db.clone()),
            surface,
            Arc::clone(&gate),
        ),
        gate,
    )
}

async fn stored_identity(db: &Database, task_run_id: &str) -> BuildPodResizeIdentity {
    BuildPodPermitRepository::new(db.clone())
        .active(task_run_id)
        .await
        .expect("read permit")
        .expect("permit row")
        .resize_identity
        .expect("resize identity")
}

// ── AC1: the stored ceiling is the value read back from the live Pod ───

/// AC1. The Job was rendered with [`RENDERED_CEILING`]; a mutating
/// admission webhook lowered the persisted launcher limit to 3600m. The
/// stored ceiling must be the MUTATED value.
///
/// Non-vacuity: substitute `RENDERED_CEILING` for the asserted 3600 and
/// this test fails — which is the point. A test written against the render
/// input would pass on an implementation that never reads the Pod at all.
#[tokio::test]
async fn the_stored_ceiling_is_the_mutated_value_the_apiserver_persisted() {
    const MUTATED_BY_WEBHOOK: u64 = 3600;
    assert_ne!(
        MUTATED_BY_WEBHOOK, RENDERED_CEILING,
        "the fixture must actually differ from the render input"
    );

    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "ac1").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-ac1").await;

    // The Pod the apiserver STORED, not the one that was rendered.
    let surface = Arc::new(FakeSurface::observing(resize_v2(
        "pod-uid-ac1",
        MUTATED_BY_WEBHOOK,
    )));
    let (bootstrap, gate) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);

    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert_eq!(
        outcome,
        BootstrapOutcome::BirthConfirmed {
            pod_uid: "pod-uid-ac1".to_owned(),
            admitted_cpu_millicores: i64::try_from(MUTATED_BY_WEBHOOK).unwrap(),
        }
    );

    let identity = stored_identity(&db, &task_run_id).await;
    assert_eq!(
        identity.admitted_cpu_millicores,
        i64::try_from(MUTATED_BY_WEBHOOK).unwrap(),
        "the stored ceiling must be the persisted value, not the render input"
    );
    assert_eq!(identity.pod_uid, "pod-uid-ac1");
    assert_eq!(identity.effective_launcher_protocol, "resize-v2");
    assert_eq!(identity.observed_launcher_protocol, "resize-v2");

    // And the birth downsize went to 250m, not to the render's ceiling.
    assert_eq!(
        surface.resizes(),
        vec![("taskrun-pod-uid-ac1".to_owned(), BIRTH_CPU_MILLICORES)]
    );
    assert!(gate.admit(&task_run_id).is_ok());
}

// ── AC2: no dispatch before the birth limit is confirmed ───────────────

/// AC2. An unconfirmed birth downsize does not admit dispatch, and the
/// gate is what stops it.
///
/// Non-vacuity: the fake dispatcher calls
/// [`DispatchGate::record_dispatch_started`] the way a real dispatch site
/// would. With the gate in front of it the counter stays zero; remove the
/// gate — dispatch unconditionally — and it becomes non-zero.
#[tokio::test]
async fn dispatch_is_refused_until_the_birth_limit_is_confirmed() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "ac2").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-ac2").await;

    let surface = Arc::new(
        FakeSurface::observing(resize_v2("pod-uid-ac2", CEILING)).with_resize_results(vec![
            Err(PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
                observed_millis: CEILING,
                target_millis: BIRTH_CPU_MILLICORES,
            })),
            Ok(()),
        ]),
    );
    let (bootstrap, gate) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);

    // Pass 1: capture succeeds, birth is NOT confirmed.
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert!(
        matches!(
            outcome,
            BootstrapOutcome::NotReady(NotReady::BirthUnconfirmed(_))
        ),
        "unexpected outcome: {outcome:?}"
    );
    assert!(!outcome.admits_dispatch());

    // The gate refuses, so the dispatch site is never reached.
    let refused = gate.admit(&task_run_id);
    assert_eq!(
        refused,
        Err(DispatchRefused::BirthNotConfirmed {
            task_run_id: task_run_id.clone(),
        })
    );
    if refused.is_ok() {
        gate.record_dispatch_started(&task_run_id);
    }
    assert_eq!(
        gate.dispatches_before_birth_confirmation(),
        0,
        "the gate must keep the dispatch site unreached"
    );

    // Pass 2 (the resumed bootstrap): the same identity replays as
    // AlreadyCaptured and the resize confirms.
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert!(outcome.admits_dispatch(), "unexpected outcome: {outcome:?}");
    let admission = gate
        .admit(&task_run_id)
        .expect("admitted after confirmation");
    assert_eq!(admission.authority(), AdmittedAuthority::Resize);
    gate.record_dispatch_started(&task_run_id);
    assert_eq!(gate.dispatches_before_birth_confirmation(), 0);

    // Two resize attempts, both at the birth limit, never at the ceiling.
    assert_eq!(
        surface.resizes(),
        vec![
            ("taskrun-pod-uid-ac2".to_owned(), BIRTH_CPU_MILLICORES),
            ("taskrun-pod-uid-ac2".to_owned(), BIRTH_CPU_MILLICORES),
        ]
    );
    // A Pod that merely has not confirmed yet is not destroyed.
    assert!(surface.deletes().is_empty());
}

/// AC2's counter is not decoration: a dispatch site reached without the
/// gate registers, which is what makes removing the gate detectable.
#[test]
fn the_counter_moves_when_a_dispatch_site_is_reached_unconfirmed() {
    let gate = DispatchGate::new();
    assert_eq!(gate.dispatches_before_birth_confirmation(), 0);
    gate.record_dispatch_started("never-bootstrapped");
    assert_eq!(gate.dispatches_before_birth_confirmation(), 1);
}

// ── AC3: the partial unique index is load-bearing ──────────────────────

/// AC3. Two DIFFERENT task runs, each holding its own permit, observe the
/// SAME Pod UID with different ceilings. The second is refused without
/// mutating the first row, and its Pod is UID-fenced deleted.
///
/// Non-vacuity: `capture_resize_identity`'s own predicate
/// (`state = 'job_created' AND pod_uid IS NULL`) is satisfied for the
/// second row — its permit has never captured anything. Only migration
/// 164's `build_pod_permits_pod_uid_key` partial unique index can see
/// across task runs. Drop that index and the second capture SUCCEEDS, both
/// permits believe they hold authority over one Pod, and this test fails.
#[tokio::test]
async fn a_second_task_run_cannot_capture_the_same_pod_uid() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let first_run = seed_task_run(&db, "ac3a").await;
    let second_run = seed_task_run(&db, "ac3b").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let first = bound_permit(&permits, &first_run, "job-uid-ac3a").await;
    let second = bound_permit(&permits, &second_run, "job-uid-ac3b").await;

    let first_surface = Arc::new(FakeSurface::observing(resize_v2("shared-pod-uid", CEILING)));
    let (first_bootstrap, _) = bootstrap_with(
        &db,
        Arc::clone(&first_surface) as Arc<dyn TaskRunPodSurface>,
    );
    assert!(
        first_bootstrap
            .bootstrap(&first, LauncherAuthorityProtocol::ResizeV2)
            .await
            .admits_dispatch()
    );

    // A different ceiling for the same Pod UID, under a different permit.
    let second_surface = Arc::new(FakeSurface::observing(resize_v2("shared-pod-uid", 2000)));
    let (second_bootstrap, second_gate) = bootstrap_with(
        &db,
        Arc::clone(&second_surface) as Arc<dyn TaskRunPodSurface>,
    );
    let outcome = second_bootstrap
        .bootstrap(&second, LauncherAuthorityProtocol::ResizeV2)
        .await;

    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::PodUidClaimedByAnotherPermit,
            disposition: PodDisposition::UidFencedDeleted(Ok(())),
        },
        "without build_pod_permits_pod_uid_key this capture succeeds"
    );
    assert_eq!(
        second_surface.deletes(),
        vec![(second_run.clone(), "shared-pod-uid".to_owned())]
    );
    assert!(second_surface.resizes().is_empty());
    assert!(second_gate.admit(&second_run).is_err());

    // The first row is untouched: same ceiling, same protocol.
    let first_identity = stored_identity(&db, &first_run).await;
    assert_eq!(
        first_identity.admitted_cpu_millicores,
        i64::try_from(CEILING).unwrap()
    );
    // The loser wrote nothing at all.
    assert!(
        BuildPodPermitRepository::new(db.clone())
            .active(&second_run)
            .await
            .expect("read loser permit")
            .expect("loser row")
            .resize_identity
            .is_none()
    );
}

/// AC3's other arm: the same task run, the same Pod UID, a DIFFERENT
/// ceiling. Caught by the per-row write-once predicate rather than the
/// index, and it must not mutate the stored row either.
#[tokio::test]
async fn a_recapture_with_a_different_ceiling_is_refused_without_mutating_the_row() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "ac3c").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-ac3c").await;

    let surface = Arc::new(FakeSurface::observing(resize_v2("pod-uid-ac3c", CEILING)));
    let (bootstrap, _) = bootstrap_with(&db, surface as Arc<dyn TaskRunPodSurface>);
    assert!(
        bootstrap
            .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
            .await
            .admits_dispatch()
    );

    // Same Pod, different ceiling. The stored ceiling is authority: the
    // resumed pass uses the ROW, so the changed spec is simply ignored and
    // the birth downsize replays. Nothing durable moves.
    let mutated = Arc::new(FakeSurface::observing(resize_v2("pod-uid-ac3c", 1000)));
    let (bootstrap, _) = bootstrap_with(&db, Arc::clone(&mutated) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert_eq!(
        outcome,
        BootstrapOutcome::BirthConfirmed {
            pod_uid: "pod-uid-ac3c".to_owned(),
            admitted_cpu_millicores: i64::try_from(CEILING).unwrap(),
        },
        "a post-bootstrap spec change must not alter the write-once ceiling"
    );
    assert_eq!(
        stored_identity(&db, &task_run_id)
            .await
            .admitted_cpu_millicores,
        i64::try_from(CEILING).unwrap()
    );
    assert!(mutated.deletes().is_empty());
}

// ── AC4: the replacement-Pod path ──────────────────────────────────────

/// AC4. A replacement Pod for the SAME task run carries a new UID. Exactly
/// one capture per task run is possible, ever, so the decision is
/// **reject-and-delete**: the replacement is UID-fenced deleted, the
/// original identity is left intact, and dispatch is refused.
///
/// Release-and-reacquire is asserted here to be unavailable, not merely
/// unchosen: releasing the permit and re-acquiring under the same task-run
/// id returns `AlreadyReleased`, by design.
#[tokio::test]
async fn a_replacement_pod_for_the_same_task_run_is_rejected_and_deleted() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "ac4").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-ac4").await;

    let original = Arc::new(FakeSurface::observing(resize_v2(
        "pod-uid-original",
        CEILING,
    )));
    let (bootstrap, _) = bootstrap_with(&db, original as Arc<dyn TaskRunPodSurface>);
    assert!(
        bootstrap
            .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
            .await
            .admits_dispatch()
    );

    let replacement = Arc::new(FakeSurface::observing(resize_v2(
        "pod-uid-replacement",
        CEILING,
    )));
    let (bootstrap, gate) =
        bootstrap_with(&db, Arc::clone(&replacement) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;

    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::ConflictingCapture,
            disposition: PodDisposition::UidFencedDeleted(Ok(())),
        }
    );
    assert_eq!(
        replacement.deletes(),
        vec![(task_run_id.clone(), "pod-uid-replacement".to_owned())]
    );
    assert!(replacement.resizes().is_empty());
    assert!(gate.admit(&task_run_id).is_err());

    // The original identity is untouched.
    assert_eq!(
        stored_identity(&db, &task_run_id).await.pod_uid,
        "pod-uid-original"
    );

    // And the alternative — release-and-reacquire — does not exist.
    let released = permits
        .release(
            &task_run_id,
            &permit.permit_id,
            permit.fencing_token,
            "replacement-pod",
        )
        .await
        .expect("release");
    assert!(
        matches!(released, djinn_db::ReleaseBuildPodPermitResult::Released(_)),
        "unexpected release outcome: {released:?}"
    );
    let reacquired = permits.acquire(&task_run_id, 16).await;
    assert!(
        matches!(
            reacquired,
            AcquireBuildPodPermitResult::AlreadyReleased { .. }
        ),
        "release-and-reacquire must be unavailable, got {reacquired:?}"
    );
}

// ── Fail-closed boundaries ─────────────────────────────────────────────

#[tokio::test]
async fn a_protocol_mismatch_refuses_and_deletes_before_any_capture() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "mismatch").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-mismatch").await;

    let surface = Arc::new(FakeSurface::observing(observed(
        "pod-uid-mismatch",
        Some(CEILING),
        Some("leaf-v1"),
    )));
    let (bootstrap, _) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);

    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::ProtocolMismatch {
                observed: "leaf-v1".to_owned(),
                effective: "resize-v2".to_owned(),
            },
            disposition: PodDisposition::UidFencedDeleted(Ok(())),
        }
    );
    assert!(
        BuildPodPermitRepository::new(db.clone())
            .active(&task_run_id)
            .await
            .expect("read permit")
            .expect("row")
            .resize_identity
            .is_none()
    );
}

#[tokio::test]
async fn a_resize_v2_pod_without_a_stored_ceiling_is_refused_and_deleted() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "noceiling").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-noceiling").await;

    let surface = Arc::new(FakeSurface::observing(observed(
        "pod-uid-noceiling",
        None,
        Some("resize-v2"),
    )));
    let (bootstrap, _) = bootstrap_with(&db, surface as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::NoAdmittedCeiling,
            disposition: PodDisposition::UidFencedDeleted(Ok(())),
        }
    );
}

#[tokio::test]
async fn a_ceiling_below_the_birth_limit_is_refused_rather_than_upsized() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "lowceiling").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-lowceiling").await;

    let surface = Arc::new(FakeSurface::observing(resize_v2("pod-uid-low", 100)));
    let (bootstrap, _) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;
    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::CeilingBelowBirthLimit {
                ceiling: 100,
                birth: BIRTH_CPU_MILLICORES,
            },
            disposition: PodDisposition::UidFencedDeleted(Ok(())),
        }
    );
    assert!(
        surface.resizes().is_empty(),
        "no resize may be issued past the admitted ceiling"
    );
}

#[tokio::test]
async fn a_leaf_v1_pod_captures_nothing_and_dispatches_under_leaf_authority() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "leaf").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-leaf").await;

    let surface = Arc::new(FakeSurface::observing(observed(
        "pod-uid-leaf",
        None,
        Some("leaf-v1"),
    )));
    let (bootstrap, gate) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::LeafV1)
        .await;

    assert_eq!(outcome, BootstrapOutcome::LeafAuthority);
    assert!(surface.resizes().is_empty());
    assert!(surface.deletes().is_empty());
    assert_eq!(
        gate.admit(&task_run_id)
            .expect("leaf admission")
            .authority(),
        AdmittedAuthority::Leaf
    );
    assert!(
        BuildPodPermitRepository::new(db.clone())
            .active(&task_run_id)
            .await
            .expect("read permit")
            .expect("row")
            .resize_identity
            .is_none(),
        "migration 164 requires admitted_cpu_millicores > 0, so leaf-v1 cannot capture"
    );
}

/// A `leaf-v1` Pod that nevertheless carries a launcher CPU limit is the
/// 7deu ancestor clamp, re-entered. Refuse and delete.
#[tokio::test]
async fn a_leaf_v1_pod_carrying_a_ceiling_is_refused_and_deleted() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "leafclamp").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-leafclamp").await;

    let surface = Arc::new(FakeSurface::observing(observed(
        "pod-uid-leafclamp",
        Some(CEILING),
        Some("leaf-v1"),
    )));
    let (bootstrap, gate) = bootstrap_with(&db, surface as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::LeafV1)
        .await;
    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::LeafAuthorityCarriesCeiling { ceiling: CEILING },
            disposition: PodDisposition::UidFencedDeleted(Ok(())),
        }
    );
    assert!(gate.admit(&task_run_id).is_err());
}

#[tokio::test]
async fn a_still_starting_launcher_is_retried_rather_than_captured_or_deleted() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "starting").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-starting").await;

    let mut starting = resize_v2("pod-uid-starting", CEILING);
    starting.launcher_container_id = None;
    let surface = Arc::new(FakeSurface::observing(starting));
    let (bootstrap, gate) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;

    assert_eq!(
        outcome,
        BootstrapOutcome::NotReady(NotReady::LauncherNotStarted {
            field: "status.initContainerStatuses[].containerID",
        })
    );
    assert!(surface.deletes().is_empty());
    assert!(surface.resizes().is_empty());
    assert!(gate.admit(&task_run_id).is_err());
}

#[tokio::test]
async fn an_unnameable_launcher_is_refused_without_a_delete_it_cannot_fence() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "ambiguous").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let permit = bound_permit(&permits, &task_run_id, "job-uid-ambiguous").await;

    let surface = Arc::new(FakeSurface::new(Err(LauncherObservationError::Ambiguous(
        PodResizeError::LauncherIdentityAmbiguous {
            site: djinn_k8s::pod_resize::LauncherIdentitySite::SpecInitContainers,
            found: 2,
        },
    ))));
    let (bootstrap, gate) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
        .await;

    assert!(
        matches!(
            outcome,
            BootstrapOutcome::Refused {
                reason: RefusalReason::LauncherNotNameable(_),
                disposition: PodDisposition::Retained,
            }
        ),
        "unexpected outcome: {outcome:?}"
    );
    assert!(surface.deletes().is_empty());
    assert!(gate.admit(&task_run_id).is_err());
}

#[tokio::test]
async fn a_stale_permit_cannot_destroy_the_live_owners_pod() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "stale").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let live = bound_permit(&permits, &task_run_id, "job-uid-stale").await;
    let stale = PermitBinding {
        fencing_token: live.fencing_token + 1,
        ..live.clone()
    };

    let surface = Arc::new(FakeSurface::observing(resize_v2("pod-uid-stale", CEILING)));
    let (bootstrap, gate) = bootstrap_with(&db, Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>);
    let outcome = bootstrap
        .bootstrap(&stale, LauncherAuthorityProtocol::ResizeV2)
        .await;

    assert_eq!(
        outcome,
        BootstrapOutcome::Refused {
            reason: RefusalReason::StalePermit,
            disposition: PodDisposition::Retained,
        }
    );
    assert!(surface.deletes().is_empty());
    assert!(gate.admit(&task_run_id).is_err());
}

#[tokio::test]
async fn a_permit_without_a_bound_job_is_not_ready_to_capture() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let task_run_id = seed_task_run(&db, "unbound").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = match permits.acquire(&task_run_id, 16).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("unexpected acquire outcome: {other:?}"),
    };
    let permit = PermitBinding {
        task_run_id: task_run_id.clone(),
        permit_id: row.permit_id,
        fencing_token: row.fencing_token,
    };

    let surface = Arc::new(FakeSurface::observing(resize_v2(
        "pod-uid-unbound",
        CEILING,
    )));
    let (bootstrap, _) = bootstrap_with(&db, surface as Arc<dyn TaskRunPodSurface>);
    assert_eq!(
        bootstrap
            .bootstrap(&permit, LauncherAuthorityProtocol::ResizeV2)
            .await,
        BootstrapOutcome::NotReady(NotReady::JobNotBound)
    );
}
