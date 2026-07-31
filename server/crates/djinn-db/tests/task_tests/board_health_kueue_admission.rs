//! Database-backed proof that the `kueue_workload_admission` projection has a
//! real reader.
//!
//! Every assertion here runs against a live Postgres and a real
//! `board_health()` call. The failure mode being guarded is the one that made
//! this task exist: a projection that is written and never read, or read by
//! something that returns a constant and therefore cannot be wrong.

use super::*;

use djinn_db::KueueWorkloadAdmissionRepository;

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

/// Create the `task_runs` row the in-pod supervisor would write once Kueue
/// admitted the Job, and return its id.
async fn seed_task_run(db: &Database, task_id: &str) -> String {
    let project_id: String = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let task_run_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
         VALUES ($1, $2, $3, 'dispatch', 'running')",
    )
    .bind(&task_run_id)
    .bind(&project_id)
    .bind(task_id)
    .execute(db.pool())
    .await
    .unwrap();
    task_run_id
}

/// Move a projection row's admission state with a direct database write, the
/// way Kueue's reflector would, and the way an operator would to check whether
/// anything is actually reading it.
async fn set_admission(db: &Database, task_run_id: &str, admission: &str) {
    let updated = sqlx::query(
        "UPDATE kueue_workload_admission SET admission = $2, observed_at = now() \
         WHERE task_run_id = $1",
    )
    .bind(task_run_id)
    .bind(admission)
    .execute(db.pool())
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(updated, 1, "the projection row must exist to be flipped");
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

/// **AC1 — non-vacuity, both directions.**
///
/// A reader that returns a constant, or that always reports `null`, passes any
/// single-state assertion. So the row is flipped `pending` → `admitted` →
/// `pending` against a live database and the surfaced value is asserted to move
/// with it every time, including the `reasons` list and the final verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_surfaced_admission_state_follows_the_row_in_both_directions() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Waiting on Kueue").await;
    let task_run_id = seed_task_run(&db, &task.id).await;

    KueueWorkloadAdmissionRepository::new(db.clone())
        .apply(&task_run_id, "pending", Some("Preempted"), Some("wl-1"))
        .await
        .unwrap();

    // ── pending ────────────────────────────────────────────────────────────
    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(
        gate["kueue_workload"]["admission"], "pending",
        "the gate must report the row's actual state, got {:?}",
        gate["kueue_workload"]
    );
    assert_eq!(gate["kueue_workload"]["task_run_id"], task_run_id);
    assert_eq!(gate["kueue_workload"]["reason"], "Preempted");
    assert_eq!(gate["kueue_workload"]["workload_name"], "wl-1");
    assert_eq!(gate["kueue_admission"]["projection_state"], "observing");
    assert_eq!(gate["kueue_admission"]["pending"], 1);
    assert_eq!(gate["kueue_admission"]["admitted"], 0);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_workload_pending")),
        "a Workload Kueue has not admitted is a named blocking reason, got {:?}",
        gate["reasons"]
    );
    assert_eq!(gate["gate_verdict"], "blocked");

    // ── admitted ───────────────────────────────────────────────────────────
    set_admission(&db, &task_run_id, "admitted").await;
    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(
        gate["kueue_workload"]["admission"], "admitted",
        "the surfaced value must move when the row moves"
    );
    assert_eq!(gate["kueue_admission"]["pending"], 0);
    assert_eq!(gate["kueue_admission"]["admitted"], 1);
    assert!(
        gate["kueue_admission"]["pending_task_runs"]
            .as_array()
            .unwrap()
            .is_empty(),
        "nothing is pending once the Workload is admitted"
    );
    assert!(
        !gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_workload_pending")),
        "an admitted Workload must not keep reporting a queue"
    );

    // ── and back, because a one-way reader is still a broken one ───────────
    set_admission(&db, &task_run_id, "pending").await;
    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["kueue_workload"]["admission"], "pending");
    assert_eq!(gate["kueue_admission"]["pending"], 1);
    assert_eq!(gate["kueue_admission"]["admitted"], 0);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_workload_pending")),
        "a re-queued Workload — Kueue preempting an admitted build — must reappear"
    );
}

/// **AC2 — the inert case is not a stall.**
///
/// Production runs `kueue.armed=false`: no namespace is Kueue-managed, the
/// reflector never starts, and the projection is empty. Reporting that as
/// pending would describe a healthy cluster as a wedged one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unarmed_deployment_reports_the_inert_variant_not_pending() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Nothing to do with Kueue").await;

    // No reflector has run, because `kueue.armed=false` is what production
    // ships: the relation is empty exactly as it is today.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kueue_workload_admission")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "the fixture must reproduce the unarmed steady state"
    );

    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(
        gate["kueue_admission"]["projection_state"], "no_workloads_observed",
        "an empty projection is the INERT variant"
    );
    assert_eq!(gate["kueue_admission"]["pending"], 0);
    assert!(gate["kueue_workload"].is_null());
    assert!(
        !gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_workload_pending")),
        "an unarmed cluster must never contribute a pending reason, got {:?}",
        gate["reasons"]
    );
    assert_eq!(
        gate["gate_verdict"], "unexplained",
        "nothing about Kueue explains this task, and the gate must not pretend otherwise"
    );

    // And the gate is honestly declared unevaluated: a reflector that never
    // started and an unarmed cluster are the same empty relation from here.
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(
        unevaluated.contains(&serde_json::json!("kueue_clusterqueue_admission")),
        "got {unevaluated:?}"
    );
    assert!(
        gate["coverage"]["kueue_admission_unevaluated_detail"]
            .as_str()
            .is_some_and(|d| d.contains("no rows")),
        "the reason the gate was not evaluated must travel with the payload"
    );
}

/// **AC3 — no phantom entries.**
///
/// A projection row whose `task_runs` row does not exist must never be
/// attributed to a task. The counts still see it — it is a real Workload — but
/// no task's `dispatch_gate` may claim it, and the listed entry carries a null
/// task rather than an invented one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_projection_row_with_no_task_run_is_attributed_to_nobody() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Innocent bystander").await;

    // A task-run id that no `task_runs` row has ever had.
    let orphan_run = uuid::Uuid::now_v7().to_string();
    KueueWorkloadAdmissionRepository::new(db.clone())
        .apply(&orphan_run, "pending", Some("Preempted"), Some("wl-orphan"))
        .await
        .unwrap();

    let gate = gate_for(&repo, &task.id).await;
    assert!(
        gate["kueue_workload"].is_null(),
        "an unattributable row must not attach itself to an unrelated task, got {:?}",
        gate["kueue_workload"]
    );
    assert!(
        !gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_workload_pending")),
        "a task with no Workload of its own must not inherit somebody else's queue"
    );
    assert_eq!(
        gate["gate_verdict"], "unexplained",
        "an orphan row is not an explanation for this task"
    );

    // Not silently dropped either: it is a real pending Workload, counted and
    // listed with an explicitly null task.
    assert_eq!(gate["kueue_admission"]["pending"], 1);
    assert_eq!(gate["kueue_admission"]["without_task_run"], 1);
    let listed = gate["kueue_admission"]["pending_task_runs"]
        .as_array()
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["task_run_id"], orphan_run);
    assert!(
        listed[0]["task_id"].is_null() && listed[0]["task_short_id"].is_null(),
        "the entry must decline to name a task rather than guess one, got {:?}",
        listed[0]
    );
}

/// **AC4 — the surface depends on the projection and on nothing else.**
///
/// Everything else this task could be blocked on is explicitly clean: the lease
/// authority is armed with spare capacity, there is no dispatch lease row, no
/// recorded denial, a healthy credential and no chosen model. The ONLY thing
/// that can produce a `blocked` verdict is the projection read. Delete the row
/// and the same board-health call returns `unexplained`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_verdict_is_carried_by_the_projection_alone() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Only Kueue can explain this").await;
    let task_run_id = seed_task_run(&db, &task.id).await;

    // An armed, NOT-full lease pool: the other durable authority has nothing to
    // say about this task.
    sqlx::query("UPDATE admission_handoff SET v1_mode = 'enforce' WHERE name = 'build'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE build_lease_caps SET cap = 8 WHERE singleton")
        .execute(db.pool())
        .await
        .unwrap();

    let baseline = gate_for(&repo, &task.id).await;
    assert_eq!(
        baseline["gate_verdict"], "unexplained",
        "with an empty projection nothing explains this task; got {:?}",
        baseline["reasons"]
    );
    assert_eq!(baseline["build_capacity"]["at_capacity"], false);
    assert!(baseline["build_lease"].is_null());
    assert!(baseline["build_admission_denial"].is_null());

    KueueWorkloadAdmissionRepository::new(db.clone())
        .apply(&task_run_id, "pending", Some("Pending"), Some("wl-only"))
        .await
        .unwrap();

    let blocked = gate_for(&repo, &task.id).await;
    assert_eq!(
        blocked["gate_verdict"], "blocked",
        "the projection is the only thing that changed, so it must be what changed the verdict"
    );
    assert_eq!(
        blocked["reasons"].as_array().unwrap(),
        &vec![serde_json::json!("kueue_workload_pending")],
        "and it must be the ONLY reason — no other gate is firing here"
    );
    let evaluated = blocked["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(
        evaluated.contains(&serde_json::json!("kueue_clusterqueue_admission")),
        "with rows in the projection the gate is genuinely evaluated; got {evaluated:?}"
    );

    // Remove the read's only input and the surface collapses back — the answer
    // is not derivable from anywhere else in the payload.
    sqlx::query("DELETE FROM kueue_workload_admission WHERE task_run_id = $1")
        .bind(&task_run_id)
        .execute(db.pool())
        .await
        .unwrap();
    let after = gate_for(&repo, &task.id).await;
    assert_eq!(after["gate_verdict"], "unexplained");
    assert!(after["kueue_workload"].is_null());
    assert_eq!(
        after["kueue_admission"]["projection_state"],
        "no_workloads_observed"
    );
}

/// A `finished` Workload is neither a queue nor a live build, so it contributes
/// no reason. Without this the reader could satisfy every test above by
/// treating any row as blocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_workload_contributes_no_reason() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = stranded_task(&db, &repo, "Workload already finished").await;
    let task_run_id = seed_task_run(&db, &task.id).await;

    KueueWorkloadAdmissionRepository::new(db.clone())
        .apply(&task_run_id, "finished", None, Some("wl-done"))
        .await
        .unwrap();

    let gate = gate_for(&repo, &task.id).await;
    assert_eq!(gate["kueue_workload"]["admission"], "finished");
    assert_eq!(gate["kueue_admission"]["finished"], 1);
    assert!(
        gate["reasons"].as_array().unwrap().is_empty(),
        "a finished Workload explains nothing, got {:?}",
        gate["reasons"]
    );
    assert_eq!(gate["gate_verdict"], "unexplained");
}
