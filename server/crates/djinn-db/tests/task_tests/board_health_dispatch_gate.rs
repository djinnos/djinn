//! Integration coverage for the honest stranded-ready dispatch gate.
//!
//! These tests exist because the previous payload was structurally incapable of
//! being wrong: `reasons` could only be populated from the model-health join,
//! so a task with no chosen model always reported `gate_verdict: "stranded"`
//! with `reasons: []`, and the doctor re-emitted that as a `critical` finding
//! every thirty seconds. Each test below fails if that behaviour returns.

use super::*;

/// Seed a stranded open task in its own project, backdated past the threshold.
async fn stranded_task(db: &Database, repo: &TaskRepository, title: &str) -> Task {
    let project = create_test_project(db).await;
    let epic = create_test_epic(db, &project.id).await;
    let task = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            title,
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(db, &task.id, "600 minutes").await;
    task
}

/// Arm the v1 lease authority and set the pool cap.
async fn arm_lease_authority(db: &Database, cap: i64) {
    sqlx::query("UPDATE admission_handoff SET v1_mode = 'enforce' WHERE name = 'build'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE build_lease_caps SET cap = $1 WHERE singleton")
        .bind(cap)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Insert a `task_dispatch` build-lease row exactly as the dispatcher's
/// layer-1 admission would.
async fn insert_dispatch_lease(db: &Database, task_id: &str, generation: i64, state: &str) {
    let consumer_id = format!("{task_id}:{generation}");
    let identity = format!("dispatch:{task_id}:{generation}");
    // The schema requires a fencing token for occupying states, forbids one on
    // queued/terminal, and requires `terminal_at` on terminal rows.
    let sql = match state {
        "queued" => {
            "INSERT INTO build_leases \
             (consumer_kind, consumer_id, immutable_identity, state, weight) \
             VALUES ('task_dispatch', $1, $2, 'queued', 1)"
        }
        "terminal" => {
            "INSERT INTO build_leases \
             (consumer_kind, consumer_id, immutable_identity, state, weight, \
              terminal_reason, terminal_at) \
             VALUES ('task_dispatch', $1, $2, 'terminal', 1, 'reclaimed_absent', now())"
        }
        _ => {
            "INSERT INTO build_leases \
             (consumer_kind, consumer_id, immutable_identity, state, weight, \
              fencing_token, granted_at) \
             VALUES ('task_dispatch', $1, $2, 'active', 1, \
                     nextval('build_lease_fencing_token_seq'), now())"
        }
    };
    sqlx::query(sql)
        .bind(&consumer_id)
        .bind(&identity)
        .execute(db.pool())
        .await
        .unwrap();
}

async fn gate_for(repo: &TaskRepository, task_id: &str) -> serde_json::Value {
    let report = repo.board_health(24).await.unwrap();
    report["stranded_ready"]["findings"]
        .as_array()
        .expect("stranded_ready.findings is an array")
        .iter()
        .find(|f| f["id"] == task_id)
        .unwrap_or_else(|| panic!("task {task_id} must appear as stranded"))["dispatch_gate"]
        .clone()
}

/// **The regression.** A task the dispatcher denied for a capacity/lease reason
/// must not report an empty-`reasons` `stranded` verdict. Before this change
/// the lease ledger was never read at all
/// (`grep -c "build_leases" board_health.rs` was 0), so this task produced
/// `gate_verdict: "stranded", reasons: []`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_dispatch_lease_is_reported_as_a_capacity_denial() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Capacity-denied task").await;

    arm_lease_authority(&db, 1).await;
    insert_dispatch_lease(&db, &task.id, 3, "queued").await;

    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(
        gate["gate_verdict"], "blocked",
        "a task holding a FIFO position was denied by the cap and must say so"
    );
    assert_ne!(
        gate["gate_verdict"], "stranded",
        "the `stranded` verdict is retired"
    );
    let reasons = gate["reasons"].as_array().unwrap();
    assert!(
        reasons.contains(&serde_json::json!("build_lease_queued")),
        "expected build_lease_queued, got {reasons:?}"
    );
    assert_eq!(gate["build_lease"]["state"], "queued");
    assert_eq!(gate["build_lease"]["consumer_id"], format!("{}:3", task.id));
    assert_eq!(gate["build_capacity"]["cap"], 1);
    assert_eq!(gate["build_capacity"]["lease_authority_enforcing"], true);
}

/// The #2661 tombstone shape: the newest `task_dispatch` attempt is terminal.
/// That is precisely what wedged dispatch for eighteen hours while this payload
/// reported nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_dispatch_lease_is_reported() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Tombstoned task").await;

    arm_lease_authority(&db, 3).await;
    insert_dispatch_lease(&db, &task.id, 1, "terminal").await;

    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["gate_verdict"], "blocked");
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("build_lease_terminal"))
    );
    assert_eq!(gate["build_lease"]["terminal_reason"], "reclaimed_absent");
}

/// A full pool explains every task waiting behind it, even the ones that never
/// got a lease row of their own. This is the signal that was missing while one
/// monopolising task starved the board.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_pool_explains_a_task_with_no_lease_row() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let victim = stranded_task(&db, &repo, "Victim task").await;
    let hog = stranded_task(&db, &repo, "Monopolising task").await;

    arm_lease_authority(&db, 1).await;
    // The hog occupies the only slot; the victim has no row at all.
    insert_dispatch_lease(&db, &hog.id, 1, "active").await;

    let gate = gate_for(&repo, &victim.id).await;
    assert_eq!(gate["gate_verdict"], "blocked");
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("build_pool_at_capacity")),
        "a victim of a full pool must be told the pool is full"
    );
    assert!(gate["build_lease"].is_null());
    assert_eq!(gate["build_capacity"]["occupancy"], 1);
    assert_eq!(gate["build_capacity"]["at_capacity"], true);
}

/// Occupy the pool with the consumer kind production still writes.
///
/// `task_invocation` is the per-invocation cgroup lease taken from inside the
/// task-run Pod, which 9oga's non-goals retain. `consumer_id` is the invocation
/// id and `immutable_identity` is `task:{task_id}:{task_run_id}:{invocation_id}`
/// — see `djinn-coordinator`'s `build_lease::identity`.
async fn insert_invocation_lease(db: &Database, task_id: &str, invocation_id: &str, weight: i64) {
    sqlx::query(
        "INSERT INTO build_leases \
         (consumer_kind, consumer_id, immutable_identity, state, weight, \
          fencing_token, granted_at) \
         VALUES ('task_invocation', $1, $2, 'active', $3, \
                 nextval('build_lease_fencing_token_seq'), now())",
    )
    .bind(invocation_id)
    .bind(format!(
        "task:{task_id}:run-{invocation_id}:{invocation_id}"
    ))
    .bind(weight)
    .execute(db.pool())
    .await
    .unwrap();
}

/// **The Kueue-cutover acceptance criterion (`plcj`).** The gate still returns a
/// BLOCKING verdict for a genuinely blocked board, derived from a source that
/// production still writes.
///
/// Every other blocking reason in this file is proven with a `task_dispatch`
/// row, and nothing constructs `LeaseIdentity::TaskDispatch` any more — the
/// pre-create dispatch reservation was stood down by the cutover, and
/// `no_dispatch_lease_can_ever_be_acquired_again` fails if a constructor
/// returns. Those tests therefore still pass while proving only that a legacy
/// row shape is rendered.
///
/// This one fills the pool with `task_invocation` leases, which the
/// per-invocation cgroup lease writes today, and asserts the verdict flips both
/// ways against the real ledger: BLOCKED while the pool is full, and back to
/// `unexplained` once it drains. A gate rewired to a constant `Ok` — or to a
/// constant `blocked` — fails one half or the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pool_filled_by_live_invocation_leases_blocks_the_gate() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let victim = stranded_task(&db, &repo, "Victim of a full invocation pool").await;
    let compiler = stranded_task(&db, &repo, "Task-run holding the cgroup lease").await;

    arm_lease_authority(&db, 2).await;
    insert_invocation_lease(&db, &compiler.id, "inv-full-pool", 2).await;

    let gate = gate_for(&repo, &victim.id).await;
    assert_eq!(
        gate["gate_verdict"], "blocked",
        "a board whose lease pool is full of LIVE invocation leases is blocked: {gate:#}"
    );
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("build_pool_at_capacity")),
        "expected build_pool_at_capacity, got {:?}",
        gate["reasons"]
    );
    assert_eq!(gate["build_capacity"]["occupancy"], 2);
    assert_eq!(gate["build_capacity"]["cap"], 2);
    assert_eq!(gate["build_capacity"]["at_capacity"], true);
    assert!(
        gate["build_lease"].is_null(),
        "the victim holds no lease row of its own; the pool alone explains it"
    );
    // The cutover's own blind spot must be declared, not implied by a healthy
    // number: Kueue suspends a Job it has not admitted, and that leaves no row
    // in any relation this section reads.
    assert!(
        gate["coverage"]["unevaluated_gates"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_clusterqueue_admission")),
        "the ClusterQueue gap must be disclosed: {:#}",
        gate["coverage"]
    );

    // Drain the pool through the same relation. The verdict must FALL — this is
    // what a constant verdict cannot do.
    sqlx::query(
        "UPDATE build_leases SET state = 'terminal', fencing_token = NULL, \
         terminal_reason = 'released', terminal_at = now() \
         WHERE consumer_kind = 'task_invocation' AND consumer_id = 'inv-full-pool'",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let gate = gate_for(&repo, &victim.id).await;
    assert_eq!(
        gate["build_capacity"]["occupancy"], 0,
        "releasing the invocation lease must move the live occupancy: {gate:#}"
    );
    assert_eq!(gate["build_capacity"]["at_capacity"], false);
    assert_eq!(
        gate["gate_verdict"], "unexplained",
        "with the pool drained no evaluated gate fires, so the verdict must fall back"
    );
}

/// Neutralisation: with the lease ledger empty and the pool not full, nothing
/// is claimed. The verdict must be `unexplained` — never `stranded` — and it
/// must ship the list of gates it did not consult, so an empty `reasons` can be
/// read as the bounded claim it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_evidence_yields_unexplained_with_declared_coverage() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Genuinely unexplained task").await;
    arm_lease_authority(&db, 3).await;

    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["gate_verdict"], "unexplained");
    assert!(gate["reasons"].as_array().unwrap().is_empty());
    let coverage = &gate["coverage"];
    assert_eq!(coverage["scope"], "partial");
    let evaluated = coverage["evaluated_gates"].as_array().unwrap();
    assert!(evaluated.contains(&serde_json::json!("build_lease_admission")));
    let unevaluated = coverage["unevaluated_gates"].as_array().unwrap();
    assert!(
        unevaluated.len() >= 5,
        "an empty `reasons` must disclose the gates it did not consult, got {unevaluated:?}"
    );
    assert!(unevaluated.contains(&serde_json::json!("slot_pool_capacity")));
}

/// A pool that is full while the v1 authority is NOT armed grants and denies
/// nothing, so it must not be blamed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unarmed_authority_is_not_blamed_for_a_full_pool() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let victim = stranded_task(&db, &repo, "Shadow-mode victim").await;
    let hog = stranded_task(&db, &repo, "Shadow-mode hog").await;

    // Cap armed, but v1_mode left at its seeded 'off'.
    sqlx::query("UPDATE build_lease_caps SET cap = 1 WHERE singleton")
        .execute(db.pool())
        .await
        .unwrap();
    insert_dispatch_lease(&db, &hog.id, 1, "active").await;

    let gate = gate_for(&repo, &victim.id).await;
    assert_eq!(gate["gate_verdict"], "unexplained");
    assert_eq!(gate["build_capacity"]["lease_authority_enforcing"], false);
    assert_eq!(
        gate["build_capacity"]["at_capacity"], true,
        "the numbers are still reported; they are simply not blamed"
    );
}

/// **`lease_authority_enforcing` tracks the invocation-lease authority in BOTH
/// directions, and never degrades to "unobservable".**
///
/// The Kueue cutover retired the v0↔v1 handoff that this field used to read.
/// The failure mode being guarded is not "it stopped compiling" — it is a field
/// that quietly latches to one value, or reports nothing at all, while the
/// public MCP `board_health` payload keeps claiming to describe build capacity.
///
/// So the state is flipped through the REAL repository (the same path
/// `djinn-server epoch` writes) and read back through the real gate, in every
/// direction: disarmed → armed → shadow → armed → absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lease_authority_enforcing_follows_the_authority_in_both_directions() {
    use djinn_db::{InvocationLeaseAuthorityRepository, InvocationLeaseMode};

    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Authority-flip task").await;
    let authority = InvocationLeaseAuthorityRepository::new(db.clone());

    let enforcing = async |repo: &TaskRepository, id: &str| {
        gate_for(repo, id).await["build_capacity"]["lease_authority_enforcing"].clone()
    };

    // Seeded baseline: disarmed.
    let row = authority.read().await.unwrap().unwrap();
    assert_eq!(row.mode, InvocationLeaseMode::Off);
    assert_eq!(enforcing(&repo, &task.id).await, serde_json::json!(false));

    // Armed.
    let row = authority
        .set_mode_and_cap(row.epoch, InvocationLeaseMode::Enforce, Some(3))
        .await
        .unwrap();
    assert_eq!(enforcing(&repo, &task.id).await, serde_json::json!(true));

    // Shadow observes but does not enforce — the field must FALL again.
    let row = authority
        .set_mode_and_cap(row.epoch, InvocationLeaseMode::Shadow, Some(3))
        .await
        .unwrap();
    assert_eq!(enforcing(&repo, &task.id).await, serde_json::json!(false));

    // And RISE again, so this is not a one-way latch.
    let row = authority
        .set_mode_and_cap(row.epoch, InvocationLeaseMode::Enforce, Some(3))
        .await
        .unwrap();
    assert_eq!(enforcing(&repo, &task.id).await, serde_json::json!(true));

    // An absent authority row is a real `false`, not an unobservable read: the
    // whole payload must keep reporting capacity for a deployment that has
    // never armed the authority.
    authority.delete_for_test().await.unwrap();
    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["build_capacity"]["lease_authority_enforcing"], false);
    assert_eq!(
        gate["build_capacity"]["authority"], "build_leases",
        "an absent authority row must not make the capacity block unobservable"
    );
    let _ = row;
}

/// **The #2661 follow-up, end to end.** The dispatcher records why it refused;
/// `board_health` reads it back.
///
/// Written through the real repository so the migration, the CHECK constraints
/// and the read query are all exercised together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persisted_denial_cause_becomes_the_reason_for_a_stranded_task() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Denied by the controller").await;

    // Everything else is healthy: an armed, empty lease pool. This is the
    // 2026-07-29 shape.
    arm_lease_authority(&db, 3).await;

    djinn_db::BuildAdmissionDenialRepository::new(db.clone())
        .record(&djinn_db::BuildAdmissionDenialRecord {
            consumer_kind: "task_dispatch".to_owned(),
            consumer_id: task.id.clone(),
            cause: "controller_not_admitting".to_owned(),
            readiness: Some("create_unknown_health".to_owned()),
            detail: None,
            // Historical decision measurement. The projection must instead
            // render the current global lease-ledger capacity.
            occupancy: None,
            cap: 3,
            server_epoch: "epoch-under-test".to_owned(),
        })
        .await
        .unwrap();

    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["gate_verdict"], "blocked", "{gate:#}");
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "build_admission_denied_controller_not_admitting"
            )),
        "the dispatcher's own reason must reach the projection: {:?}",
        gate["reasons"]
    );
    let denial = &gate["build_admission_denial"];
    assert_eq!(denial["cause"], "controller_not_admitting");
    // The field that existed only in container logs on the node.
    assert_eq!(denial["readiness"], "create_unknown_health");
    assert_eq!(denial["server_epoch"], "epoch-under-test");
    assert_eq!(denial["denial_count"], 1);
    assert_eq!(denial["fresh"], true);
    assert_eq!(denial["scope"], "global");
    assert_eq!(denial["authority"], "build_leases");
    assert_eq!(
        denial["occupancy"], 0,
        "the empty global lease ledger, not the historical denial measurement"
    );
    assert_eq!(denial["cap"], 3);

    // A repeat denial keeps the streak start and counts the tick.
    djinn_db::BuildAdmissionDenialRepository::new(db.clone())
        .record(&djinn_db::BuildAdmissionDenialRecord {
            consumer_kind: "task_dispatch".to_owned(),
            consumer_id: task.id.clone(),
            cause: "controller_not_admitting".to_owned(),
            readiness: Some("create_unknown_health".to_owned()),
            detail: None,
            occupancy: None,
            cap: 3,
            server_epoch: "epoch-under-test".to_owned(),
        })
        .await
        .unwrap();
    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(
        gate["build_admission_denial"]["denial_count"], 2,
        "repeat denials count the streak rather than resetting it"
    );
    assert_eq!(
        gate["build_admission_denial"]["first_denied_at"], denial["first_denied_at"],
        "the streak start must survive a repeat"
    );
}

/// A legacy denial is evidence about an earlier admission decision, not the
/// authority for the capacity it reports today. In particular, inspecting a
/// task in one project must not scope its denial's occupancy to that project:
/// the build-lease ledger and cap are global.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_denial_reports_global_weighted_occupancy_from_other_projects() {
    use djinn_db::{
        BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, GrantNextBuildLeaseResult,
        QueueBuildLeaseInput,
    };

    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let inspected = stranded_task(&db, &repo, "Inspected project denial").await;
    // `stranded_task` creates a new project on every call, so these occupants
    // unambiguously belong to a different project from the inspected task.
    let outside_project_first = stranded_task(&db, &repo, "Outside project build one").await;
    let outside_project_second = stranded_task(&db, &repo, "Outside project build two").await;
    assert_ne!(inspected.project_id, outside_project_first.project_id);
    assert_ne!(inspected.project_id, outside_project_second.project_id);

    const GLOBAL_CAP: i64 = 7;
    const WEIGHTED_HELD: i64 = 5;
    arm_lease_authority(&db, GLOBAL_CAP).await;

    // Exercise the real ledger repository, not a raw fixture insert. Two
    // occupying rows with weights 2 and 3 prove the projection uses SUM(weight)
    // rather than row count; neither identity names the inspected task.
    let leases = BuildLeaseRepository::new(db.clone());
    for (task, invocation_id, weight) in [
        (&outside_project_first, "outside-project-invocation-one", 2),
        (&outside_project_second, "outside-project-invocation-two", 3),
    ] {
        leases
            .queue(&QueueBuildLeaseInput {
                key: BuildLeaseKey {
                    consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
                    consumer_id: invocation_id.to_owned(),
                },
                immutable_identity: format!("task:{}:run-{invocation_id}:{invocation_id}", task.id),
                queue_deadline: None,
                launch_deadline: None,
                weight,
            })
            .await
            .unwrap();
        assert!(matches!(
            leases
                .grant_next(GLOBAL_CAP, "2026-01-01T00:00:00Z", None)
                .await
                .unwrap(),
            GrantNextBuildLeaseResult::Granted(_)
        ));
    }
    let snapshot = leases.snapshot().await.unwrap();
    assert_eq!(snapshot.occupied, WEIGHTED_HELD);
    assert_eq!(
        snapshot.rows.iter().filter(|row| row.weight > 0).count(),
        2,
        "the held weighted occupancy must differ from the occupying row count"
    );
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| !row.immutable_identity.contains(&inspected.id)),
        "the inspected project owns no occupying lease, so a project-scoped ledger would be zero"
    );

    // Persist an intentionally stale, project-scoped-looking measurement. The
    // renderer must replace both values with the live global ledger snapshot.
    djinn_db::BuildAdmissionDenialRepository::new(db.clone())
        .record(&djinn_db::BuildAdmissionDenialRecord {
            consumer_kind: "task_dispatch".to_owned(),
            consumer_id: inspected.id.clone(),
            cause: "at_capacity".to_owned(),
            readiness: None,
            detail: None,
            occupancy: Some(0),
            cap: 1,
            server_epoch: "legacy-scoped-measurement".to_owned(),
        })
        .await
        .unwrap();

    let gate = gate_for(&repo, &inspected.id).await;
    let denial = &gate["build_admission_denial"];
    assert_eq!(denial["scope"], "global");
    assert_eq!(denial["authority"], "build_leases");
    assert_eq!(denial["occupancy"], WEIGHTED_HELD);
    assert_eq!(denial["cap"], GLOBAL_CAP);
    assert_ne!(denial["occupancy"], 0, "must not use project-scoped zero");
    assert_ne!(denial["cap"], 1, "must not use the persisted stale cap");
}

/// **Neutralisation guard: this table must not become the #2661 tombstone.**
///
/// #2661 was a spent row replayed forever. A denial record that outlived its
/// condition would be the same bug in a new table, so the permitted path
/// deletes it — and after that this section must claim nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_a_denial_stops_it_being_reported_as_a_reason() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Denied then admitted").await;
    arm_lease_authority(&db, 3).await;

    let denials = djinn_db::BuildAdmissionDenialRepository::new(db.clone());
    denials
        .record(&djinn_db::BuildAdmissionDenialRecord {
            consumer_kind: "task_dispatch".to_owned(),
            consumer_id: task.id.clone(),
            cause: "at_capacity".to_owned(),
            readiness: None,
            detail: None,
            occupancy: Some(3),
            cap: 3,
            server_epoch: "epoch-under-test".to_owned(),
        })
        .await
        .unwrap();
    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["gate_verdict"], "blocked");
    let denial = &gate["build_admission_denial"];
    assert_eq!(denial["scope"], "global");
    assert_eq!(denial["authority"], "build_leases");
    assert_eq!(
        denial["occupancy"], 0,
        "the persisted denial's historical full-pool measurement is not current capacity"
    );
    assert_eq!(denial["cap"], 3);

    // The permitted path.
    denials.clear("task_dispatch", &task.id).await.unwrap();

    let gate = gate_for(&repo, &task.id).await;
    assert!(
        gate["build_admission_denial"].is_null(),
        "a cleared denial must leave nothing behind: {gate:#}"
    );
    assert!(
        !gate["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason
                .as_str()
                .is_some_and(|reason| reason.starts_with("build_admission_denied_"))),
        "a cleared denial must not survive as a reason: {:?}",
        gate["reasons"]
    );
    // The gate is still EVALUATED — absence is an answer, not a gap.
    assert!(
        gate["coverage"]["evaluated_gates"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("build_admission_denial"))
    );
}

/// Defect 2. `bx1f` was reported stranded for 18.2 h although its blocker only
/// merged eight hours earlier. It had never been dispatched (no open
/// transition, no session), so the clock fell back to creation time and the
/// section never reset it when the blocker cleared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_strand_clock_restarts_when_the_last_blocker_clears() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;

    let blocked = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "Was blocked for most of its life",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let blocker = repo
        .create_fixture_in_project(
            &project.id,
            Some(&epic.id),
            "The blocker",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    repo.add_blocker(&blocked.id, &blocker.id).await.unwrap();

    // The blocked task was created 18 hours ago and never dispatched.
    backdate_task_updated_at(&db, &blocked.id, "1080 minutes").await;
    // Its blocker closed 8 hours ago.
    sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
        .bind(&blocker.id)
        .execute(db.pool())
        .await
        .unwrap();
    backdate_task_updated_at(&db, &blocker.id, "480 minutes").await;
    sqlx::query(
        "INSERT INTO activity_log (id, task_id, event_type, payload, created_at) \
         VALUES ($1, $2, 'status_changed', $3::jsonb, \
                 to_char(now() AT TIME ZONE 'utc' - '480 minutes'::interval, \
                         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&blocker.id)
    .bind(r#"{"to_status":"closed"}"#)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let finding = report["stranded_ready"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == blocked.id)
        .expect("an unblocked stale task must appear as stranded")
        .clone();

    assert_eq!(
        finding["unclaimed_since_basis"], "blocker_cleared",
        "the clock must start when the last blocker cleared"
    );
    assert_eq!(finding["unclaimed_since_confidence"], "high");
    let elapsed = finding["elapsed_minutes"].as_i64().unwrap();
    assert!(
        (470..=500).contains(&elapsed),
        "expected ~480 minutes of real exposure, not the ~1080 since creation; got {elapsed}"
    );
    assert_eq!(
        finding["severity"], "critical",
        "8 hours is still critical — the fix corrects the number, it does not hide the task"
    );
}

/// Neutralisation guard for the fix above: with no blocker at all the clock is
/// unchanged, so the reset cannot be masking a shortened window for every task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unblocked_task_keeps_its_original_clock() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Never blocked").await;

    let report = repo.board_health(24).await.unwrap();
    let finding = report["stranded_ready"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == task.id)
        .expect("task must appear as stranded")
        .clone();
    assert_eq!(finding["unclaimed_since_basis"], "task_updated_at");
    assert_eq!(finding["unclaimed_since_confidence"], "low");
    let elapsed = finding["elapsed_minutes"].as_i64().unwrap();
    assert!(
        (590..=620).contains(&elapsed),
        "an unblocked task keeps its full 600-minute window; got {elapsed}"
    );
}
