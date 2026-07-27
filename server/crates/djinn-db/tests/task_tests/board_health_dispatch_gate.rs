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
    assert_eq!(gate["build_capacity"]["enforcing"], true);
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
    assert_eq!(gate["build_capacity"]["enforcing"], false);
    assert_eq!(
        gate["build_capacity"]["at_capacity"], true,
        "the numbers are still reported; they are simply not blamed"
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
