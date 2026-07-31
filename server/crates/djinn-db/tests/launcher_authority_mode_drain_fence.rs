//! The authority-mode flip, its drain fence, and the pre-dispatch admission
//! point — against real PostgreSQL.
//!
//! Every fence test here runs on `Database::ephemeral()`, a real migrated
//! Postgres database. None of them substitutes a fake or in-memory repository
//! for the thing under test: the property being asserted is that a `FOR UPDATE`
//! row lock serializes two transactions, and no fake can be wrong about that in
//! the way production would be.

use std::sync::Arc;
use std::time::Duration;

use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildPodPermitRepository,
    BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, Database,
    LauncherAuthorityDrainCensus, LauncherAuthorityModeRepository, LauncherProtocolAdmission,
    SetLauncherAuthorityModeResult, decide_launcher_protocol_admission,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;

const LEAF: LauncherAuthorityProtocol = LauncherAuthorityProtocol::LeafV1;
const RESIZE: LauncherAuthorityProtocol = LauncherAuthorityProtocol::ResizeV2;

/// Seed the FK chain `users -> projects -> tasks -> task_runs` so real permit
/// rows can exist. `build_pod_permits.task_run_id` is a restricted foreign key,
/// so a drain dimension cannot be faked by inserting a bare row.
async fn seed_runs(db: &Database, ids: &[&str]) {
    db.ensure_initialized().await.unwrap();
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('00000000-0000-7000-8000-000000000167', 9000000167, 'authority-mode-fence')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('authority-mode-project', 'authority-mode-project', 'djinnos', 'authority-mode')",
    )
    .execute(pool)
    .await
    .unwrap();
    for (index, id) in ids.iter().enumerate() {
        let task_id = format!("authority-mode-task-{index}");
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ($1, 'authority-mode-project', $2, 'title', 'description', 'design', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '00000000-0000-7000-8000-000000000167')",
        )
        .bind(&task_id)
        .bind(format!("auth-{index}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
             VALUES ($1, 'authority-mode-project', $2, 'manual', 'running')",
        )
        .bind(id)
        .bind(&task_id)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn resize_identity(pod_uid: &str, protocol: LauncherAuthorityProtocol) -> BuildPodResizeIdentity {
    BuildPodResizeIdentity {
        pod_namespace: "djinn".into(),
        pod_name: format!("pod-{pod_uid}"),
        pod_uid: pod_uid.into(),
        launcher_container_name: "cgroup-launcher".into(),
        launcher_container_id: format!("containerd://{pod_uid}"),
        image_digest: "sha256:0123456789abcdef".into(),
        observed_launcher_protocol: protocol.as_wire().into(),
        effective_launcher_protocol: protocol.as_wire().into(),
        admitted_cpu_millicores: 4000,
    }
}

/// Drive one permit all the way into a nonterminal resize state, which is drain
/// dimension two.
async fn park_in_nonterminal_resize(
    permits: &BuildPodPermitRepository,
    task_run_id: &str,
    pod_uid: &str,
) {
    let row = match permits.acquire(task_run_id, 8).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("expected an acquired permit, got {outcome:?}"),
    };
    assert!(matches!(
        permits
            .bind_or_refresh_job_uid(task_run_id, &row.permit_id, row.fencing_token, "job-uid")
            .await
            .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));
    assert!(matches!(
        permits
            .capture_resize_identity(
                task_run_id,
                &row.permit_id,
                row.fencing_token,
                &resize_identity(pod_uid, RESIZE),
            )
            .await
            .unwrap(),
        CaptureBuildPodResizeIdentityResult::Captured(_)
    ));
}

async fn current(modes: &LauncherAuthorityModeRepository) -> (LauncherAuthorityProtocol, i64) {
    let row = modes
        .read()
        .await
        .unwrap()
        .expect("migration 167 seeds the singleton");
    (row.mode, row.epoch)
}

/// Activation then rollback, each from a drained snapshot, each bumping the CAS
/// fence exactly once.
///
/// MUTATION: drop `epoch = epoch + 1` from the `UPDATE` in `set_mode_inner` and
/// the epoch assertions fail — a flip that does not move the fence lets a stale
/// operator write replay on top of it.
#[tokio::test]
async fn flip_forward_then_roll_back_from_a_drained_snapshot() {
    let db = Database::ephemeral().await.unwrap();
    let modes = LauncherAuthorityModeRepository::new(db.clone());

    // Migration 167 seeds the pre-existing behavior, never the new one.
    let (mode, epoch) = current(&modes).await;
    assert_eq!(
        mode, LEAF,
        "a fresh deployment must keep launcher authority"
    );
    assert_eq!(epoch, 0);

    // leaf-v1 -> resize-v2 (activation).
    let flipped = modes.set_mode(epoch, RESIZE).await;
    let SetLauncherAuthorityModeResult::Flipped {
        row,
        previous,
        drain,
    } = flipped
    else {
        panic!("expected a flip from a drained snapshot, got {flipped:?}");
    };
    assert_eq!(previous, LEAF);
    assert_eq!(row.mode, RESIZE);
    assert_eq!(row.epoch, 1);
    assert_eq!(drain, LauncherAuthorityDrainCensus::default());
    assert!(drain.is_drained() && drain.total() == 0);
    assert_eq!(current(&modes).await, (RESIZE, 1));

    // resize-v2 -> leaf-v1 (rollback), from the epoch the flip left behind.
    let rolled_back = modes.set_mode(1, LEAF).await;
    let SetLauncherAuthorityModeResult::Flipped { row, previous, .. } = rolled_back else {
        panic!("expected a rollback, got {rolled_back:?}");
    };
    assert_eq!(previous, RESIZE);
    assert_eq!(row.mode, LEAF);
    assert_eq!(row.epoch, 2);
    assert_eq!(current(&modes).await, (LEAF, 2));

    // The fence is a fence in both directions: replaying the activation at the
    // now-stale epoch is refused and changes nothing.
    let stale = modes.set_mode(1, RESIZE).await;
    let SetLauncherAuthorityModeResult::EpochConflict { row } = stale else {
        panic!("expected a stale-epoch refusal, got {stale:?}");
    };
    assert_eq!(row.epoch, 2);
    assert_eq!(current(&modes).await, (LEAF, 2));
}

/// A same-mode replay is idempotent — but only after passing the identical
/// epoch check and drain fence a real flip passes.
///
/// MUTATION: hoist the `current.mode == next` early return above the drain
/// census in `set_mode_inner` and the second half of this test fails: the
/// replay returns `Unchanged` while a live permit is still occupying the pool,
/// which is precisely the "validation bypassed by a no-op" hole.
#[tokio::test]
async fn same_mode_replay_is_idempotent_but_never_skips_the_fence() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["replay-occupant"]).await;
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let permits = BuildPodPermitRepository::new(db.clone());

    // Drained: the replay is a no-op that neither writes nor bumps the epoch.
    let replay = modes.set_mode(0, LEAF).await;
    let SetLauncherAuthorityModeResult::Unchanged { row, drain } = replay else {
        panic!("expected an idempotent replay, got {replay:?}");
    };
    assert_eq!(row.mode, LEAF);
    assert_eq!(row.epoch, 0, "an idempotent replay must not move the fence");
    assert!(drain.is_drained());
    assert_eq!(current(&modes).await, (LEAF, 0));

    // Occupied: the SAME no-op request is now refused with the same verdict a
    // real flip would get.
    assert!(matches!(
        permits.acquire("replay-occupant", 8).await,
        AcquireBuildPodPermitResult::Acquired { .. }
    ));
    let blocked = modes.set_mode(0, LEAF).await;
    let SetLauncherAuthorityModeResult::DrainNotEmpty { row, drain } = blocked else {
        panic!("a replay must not bypass the drain fence, got {blocked:?}");
    };
    assert_eq!(row.mode, LEAF);
    assert_eq!(drain.pending_pod_permits, 1);
    assert_eq!(current(&modes).await, (LEAF, 0));
}

/// Drain dimension one — a live task-run Pod holding a permit with no resize
/// lifecycle — blocks a flip on its own, with dimension two at zero.
///
/// MUTATION: weaken `LauncherAuthorityDrainCensus::is_drained` to
/// `self.nonterminal_resize_leases == 0` and this flip succeeds while a
/// scheduled Pod is still live.
#[tokio::test]
async fn a_pending_pod_permit_alone_refuses_the_flip() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["pending-acquired", "pending-bound"]).await;
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let permits = BuildPodPermitRepository::new(db.clone());

    // `acquired` — a permit taken before the Job exists.
    assert!(matches!(
        permits.acquire("pending-acquired", 8).await,
        AcquireBuildPodPermitResult::Acquired { .. }
    ));
    // `job_created` — a Job observed, no resize identity captured yet.
    let bound = match permits.acquire("pending-bound", 8).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    assert!(matches!(
        permits
            .bind_or_refresh_job_uid(
                "pending-bound",
                &bound.permit_id,
                bound.fencing_token,
                "job-uid"
            )
            .await
            .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));

    let census = modes.drain_census().await.unwrap();
    assert_eq!(
        census,
        LauncherAuthorityDrainCensus {
            pending_pod_permits: 2,
            nonterminal_resize_leases: 0,
        },
        "dimension one must be nonzero with dimension two independently at zero"
    );

    let refused = modes.set_mode(0, RESIZE).await;
    let SetLauncherAuthorityModeResult::DrainNotEmpty { drain, .. } = refused else {
        panic!("expected the flip to be refused, got {refused:?}");
    };
    assert_eq!(drain.pending_pod_permits, 2);
    assert_eq!(drain.nonterminal_resize_leases, 0);
    assert_eq!(
        current(&modes).await,
        (LEAF, 0),
        "a refused flip must not change the mode or the fence"
    );
}

/// Drain dimension two — a nonterminal resize/lease lifecycle row — blocks a
/// flip on its own, with dimension one at zero.
///
/// MUTATION: weaken `is_drained` to `self.pending_pod_permits == 0` and this
/// flip succeeds while a Pod still owes a drop. Equivalently, delete a state
/// from `NONTERMINAL_RESIZE_STATES` and the row silently reclassifies into
/// dimension one, which this test's exact-census assertion also catches.
#[tokio::test]
async fn a_nonterminal_resize_lease_alone_refuses_the_flip() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["resize-occupant"]).await;
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    let permits = BuildPodPermitRepository::new(db.clone());

    park_in_nonterminal_resize(&permits, "resize-occupant", "pod-uid-a").await;

    let census = modes.drain_census().await.unwrap();
    assert_eq!(
        census,
        LauncherAuthorityDrainCensus {
            pending_pod_permits: 0,
            nonterminal_resize_leases: 1,
        },
        "dimension two must be nonzero with dimension one independently at zero"
    );
    assert!(!census.is_drained());

    let refused = modes.set_mode(0, RESIZE).await;
    let SetLauncherAuthorityModeResult::DrainNotEmpty { drain, .. } = refused else {
        panic!("expected the flip to be refused, got {refused:?}");
    };
    assert_eq!(drain.pending_pod_permits, 0);
    assert_eq!(drain.nonterminal_resize_leases, 1);
    assert_eq!(current(&modes).await, (LEAF, 0));

    // Draining that dimension — an explicit fenced release — reopens the flip.
    let row = permits.active("resize-occupant").await.unwrap().unwrap();
    permits
        .release(
            "resize-occupant",
            &row.permit_id,
            row.fencing_token,
            "drained",
        )
        .await
        .unwrap();
    assert!(modes.drain_census().await.unwrap().is_drained());
    assert!(matches!(
        modes.set_mode(0, RESIZE).await,
        SetLauncherAuthorityModeResult::Flipped { .. }
    ));
}

/// The fence is transactional, not a lucky call order.
///
/// A concurrent admission holds the `build_pod_permit_pools` row lock — exactly
/// what `BuildPodPermitRepository::acquire` holds while it counts and inserts —
/// and inserts a permit without committing. The flip must not be able to
/// observe a zero census in that window; it must block on the same lock and,
/// once admission commits, see the row.
///
/// MUTATION: remove `FOR UPDATE` from the pool `SELECT` in `set_mode_inner`.
/// The flip then completes immediately inside the window, the
/// `tokio::time::timeout` returns `Ok`, and this test fails on the "must not
/// have completed" assertion — the flip having committed a mode change over a
/// task-run Pod that was already being admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_admission_cannot_be_raced_by_a_zero_count() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["racing-admission"]).await;
    let modes = Arc::new(LauncherAuthorityModeRepository::new(db.clone()));

    assert!(
        modes.drain_census().await.unwrap().is_drained(),
        "the race starts from a genuinely empty pool"
    );

    // Stand in for an in-flight `acquire`: same lock, same insert, uncommitted.
    let mut admission = db.pool().begin().await.unwrap();
    sqlx::query("SELECT pool_key FROM build_pod_permit_pools WHERE pool_key = 'global' FOR UPDATE")
        .execute(&mut *admission)
        .await
        .unwrap();
    sqlx::query("INSERT INTO build_pod_permits (task_run_id) VALUES ('racing-admission')")
        .execute(&mut *admission)
        .await
        .unwrap();

    let mut flip = tokio::spawn({
        let modes = Arc::clone(&modes);
        async move { modes.set_mode(0, RESIZE).await }
    });

    // The flip must be BLOCKED, not merely late. An unfenced read would have
    // committed a mode change several orders of magnitude faster than this.
    if let Ok(joined) = tokio::time::timeout(Duration::from_millis(750), &mut flip).await {
        panic!(
            "the flip observed the pool without waiting on the admission lock and returned {:?}",
            joined.unwrap()
        );
    }

    // Release the lock; the already-blocked flip now proceeds and must see the
    // row that was invisible to it a moment ago.
    admission.commit().await.unwrap();
    let flip = tokio::time::timeout(Duration::from_secs(15), flip)
        .await
        .expect("the flip must proceed once the admission lock is released")
        .unwrap();

    let SetLauncherAuthorityModeResult::DrainNotEmpty { drain, .. } = flip else {
        panic!("the committed admission must refuse the flip, got {flip:?}");
    };
    assert_eq!(drain.pending_pod_permits, 1);
    assert_eq!(
        current(&modes).await,
        (LEAF, 0),
        "no mode change may survive a raced admission"
    );
}

/// A repository error is never an empty drain.
///
/// MUTATION A: make `drain_census` swallow its error (`.unwrap_or_default()`).
/// The preflight diagnostic then reports a fully drained pool for a relation it
/// could not read, and the `is_err()` assertion below fails.
///
/// MUTATION B: drop the `pool.is_none()` guard in `set_mode_inner`. The flip
/// then commits against a fence it never actually held — the third block below
/// fails with `Flipped` where `Unavailable` is required.
///
/// Note on what this test does NOT prove on its own: replacing the `?` on
/// `drain_census_tx` INSIDE `set_mode_inner` with `.unwrap_or_default()` is
/// survivable, because a failed statement aborts the enclosing PostgreSQL
/// transaction and the subsequent `UPDATE` errors anyway. That is defense in
/// depth from the engine, not from the code, and it evaporates if the census is
/// ever hoisted out of the flip transaction. The `?` is load-bearing for that
/// future, and the comment at the call site says so.
#[tokio::test]
async fn an_unreadable_permit_relation_is_unavailable_and_never_drained() {
    let db = Database::ephemeral().await.unwrap();
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    assert!(modes.drain_census().await.unwrap().is_drained());

    sqlx::query("DROP TABLE build_pod_permits CASCADE")
        .execute(db.pool())
        .await
        .unwrap();

    // The read-only diagnostic surfaces the failure as an error, never as a
    // zero census a caller could mistake for "drained".
    assert!(
        modes.drain_census().await.is_err(),
        "an unreadable census must not resolve to zero"
    );
    assert_eq!(
        modes.set_mode(0, RESIZE).await,
        SetLauncherAuthorityModeResult::Unavailable
    );
    assert_eq!(current(&modes).await, (LEAF, 0));

    // The pool relation is the fence itself; losing it is equally unavailable.
    let db = Database::ephemeral().await.unwrap();
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    BuildPodPermitRepository::new(db.clone())
        .drop_pool_relation_for_test()
        .await
        .unwrap();
    assert_eq!(
        modes.set_mode(0, RESIZE).await,
        SetLauncherAuthorityModeResult::Unavailable
    );
    assert_eq!(current(&modes).await, (LEAF, 0));

    // A present relation with no singleton row is also unfenced, not empty.
    let db = Database::ephemeral().await.unwrap();
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    BuildPodPermitRepository::new(db.clone())
        .delete_global_pool_for_test()
        .await
        .unwrap();
    assert_eq!(
        modes.set_mode(0, RESIZE).await,
        SetLauncherAuthorityModeResult::Unavailable
    );
    assert_eq!(current(&modes).await, (LEAF, 0));
}

/// An absent authority row is not a default mode.
///
/// MUTATION: have `TryFrom<ModeDbRow>` or the `None` arm fall back to
/// `LauncherAuthorityProtocol::default()` and an unseeded deployment silently
/// admits `leaf-v1` Pods and flips without ever having been configured.
#[tokio::test]
async fn an_absent_authority_row_is_uninitialized_not_a_default() {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    let modes = LauncherAuthorityModeRepository::new(db.clone());
    sqlx::query("DELETE FROM launcher_authority_mode WHERE mode_key = 'global'")
        .execute(db.pool())
        .await
        .unwrap();

    assert_eq!(modes.read().await.unwrap(), None);
    assert_eq!(
        modes.set_mode(0, RESIZE).await,
        SetLauncherAuthorityModeResult::Uninitialized
    );
    assert_eq!(
        modes.admit_declared_protocol(Some(LEAF.as_wire())).await,
        LauncherProtocolAdmission::AuthorityUnavailable
    );

    // An unreadable relation is likewise a refusal, not an admission.
    sqlx::query("DROP TABLE launcher_authority_mode")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(modes.read().await.is_err());
    assert_eq!(
        modes.admit_declared_protocol(Some(RESIZE.as_wire())).await,
        LauncherProtocolAdmission::AuthorityUnavailable
    );
}

/// The pre-dispatch admission point, read through the durable mode.
///
/// MUTATION: relax `decide_launcher_protocol_admission` so a mismatch or an
/// unparseable declaration falls through to `Admitted`, and the accumulated
/// list below names every input that was let through.
#[tokio::test]
async fn admission_admits_exactly_the_configured_authority() {
    let db = Database::ephemeral().await.unwrap();
    let modes = LauncherAuthorityModeRepository::new(db.clone());

    // Under leaf authority.
    assert_eq!(
        modes.admit_declared_protocol(Some("leaf-v1")).await,
        LauncherProtocolAdmission::Admitted { mode: LEAF }
    );
    assert_eq!(
        modes.admit_declared_protocol(Some("resize-v2")).await,
        LauncherProtocolAdmission::ProtocolMismatch {
            mode: LEAF,
            declared: RESIZE,
        }
    );

    // Under resize authority, after a real fenced flip.
    assert!(matches!(
        modes.set_mode(0, RESIZE).await,
        SetLauncherAuthorityModeResult::Flipped { .. }
    ));
    assert_eq!(
        modes.admit_declared_protocol(Some("resize-v2")).await,
        LauncherProtocolAdmission::Admitted { mode: RESIZE }
    );
    assert_eq!(
        modes.admit_declared_protocol(Some("leaf-v1")).await,
        LauncherProtocolAdmission::ProtocolMismatch {
            mode: RESIZE,
            declared: LEAF,
        }
    );

    // Nothing outside the closed set is ever admitted, in either mode. The
    // near-misses are what a Job template, a shell, or a YAML scalar actually
    // produces.
    let mut admitted = Vec::new();
    for mode in LauncherAuthorityProtocol::ALL {
        for declared in [
            None,
            Some(""),
            Some(" "),
            Some("leafv1"),
            Some("LEAF-V1"),
            Some("leaf_v1"),
            Some("resize-v3"),
            Some("RESIZE-V2"),
            Some(" leaf-v1"),
            Some("resize-v2 "),
            Some("leaf-v1,resize-v2"),
            Some("unknown-v3"),
        ] {
            let verdict = decide_launcher_protocol_admission(mode, declared);
            if verdict.is_admitted() {
                admitted.push((mode, declared));
            }
            match (declared, &verdict) {
                (None, LauncherProtocolAdmission::UndeclaredProtocol { .. }) => {}
                (Some(raw), LauncherProtocolAdmission::UnknownProtocol { declared, .. }) => {
                    assert_eq!(declared, raw, "the refusal must name the offending value");
                }
                (declared, verdict) => {
                    panic!("{declared:?} under {mode} produced an unexpected verdict {verdict:?}")
                }
            }
        }
    }
    assert!(
        admitted.is_empty(),
        "{} malformed or absent declarations were admitted: {admitted:?}",
        admitted.len()
    );
}

/// Migration 167's `CHECK ... IN (...)` list is derived from, and must equal,
/// [`LauncherAuthorityProtocol::ALL`].
///
/// MUTATION: add a third variant to the enum without teaching migration 167
/// about it and this fails here, rather than in production on the first `CHECK`
/// violation from an operator flipping to a mode the database refuses to store.
#[test]
fn migration_167_constrains_exactly_the_protocol_wire_forms() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations_postgres/167_launcher_authority_mode.sql");
    let sql = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("migration 167 must be readable at {path:?}: {error}"));

    let needle = "mode IN (";
    let start = sql
        .find(needle)
        .expect("migration 167 must constrain `mode` with an IN list")
        + needle.len();
    let end = start + sql[start..].find(')').expect("the IN list must be closed");
    let mut constrained: Vec<String> = sql[start..end]
        .split(',')
        .map(|value| value.trim().trim_matches('\'').to_owned())
        .collect();
    constrained.sort();

    let mut expected: Vec<String> = LauncherAuthorityProtocol::ALL
        .iter()
        .map(|protocol| protocol.as_wire().to_owned())
        .collect();
    expected.sort();

    assert_eq!(constrained, expected);
    assert!(
        sql.contains("VALUES ('global', 'leaf-v1')"),
        "the seed must be the pre-existing launcher behavior, never the new one"
    );
}
