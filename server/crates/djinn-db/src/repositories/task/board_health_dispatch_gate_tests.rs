use super::*;

fn capacity(occupancy: i64, cap: i64, enforcing: bool) -> LeaseCapacity {
    LeaseCapacity {
        occupancy,
        cap,
        enforcing,
    }
}

fn lease_row(state: &str) -> DispatchLeaseRow {
    DispatchLeaseRow {
        consumer_id: "task-1:7".to_owned(),
        state: state.to_owned(),
        terminal_reason: None,
        weight: 1,
        enqueue_sequence: 42,
        created_at: Some("2026-07-27T00:00:00.000Z".to_owned()),
        updated_at: Some("2026-07-27T00:10:00.000Z".to_owned()),
        queued_ahead: 2,
    }
}

fn observed(rows: Vec<(&str, DispatchLeaseRow)>, capacity: LeaseCapacity) -> LeaseLedger {
    observed_full(rows, capacity, Some(HashMap::new()))
}

fn observed_full(
    rows: Vec<(&str, DispatchLeaseRow)>,
    capacity: LeaseCapacity,
    denials: Option<HashMap<String, DenialRow>>,
) -> LeaseLedger {
    LeaseLedger::Observed {
        by_task: rows
            .into_iter()
            .map(|(id, row)| (id.to_owned(), row))
            .collect(),
        capacity,
        denials,
    }
}

fn denial_row(cause: &str, readiness: Option<&str>, age_seconds: i64) -> DenialRow {
    DenialRow {
        cause: cause.to_owned(),
        readiness: readiness.map(str::to_owned),
        detail: None,
        occupancy: None,
        cap: 3,
        server_epoch: "epoch-1".to_owned(),
        first_denied_at: Some("2026-07-29T06:22:57.000Z".to_owned()),
        denied_at: Some("2026-07-29T11:30:00.000Z".to_owned()),
        denial_count: 152,
        age_seconds,
    }
}

fn denials(entries: Vec<(&str, DenialRow)>) -> Option<HashMap<String, DenialRow>> {
    Some(
        entries
            .into_iter()
            .map(|(id, row)| (id.to_owned(), row))
            .collect(),
    )
}

/// The projection state production ships in: `kueue.armed=false`, no Workload
/// ever observed. Every lease-focused test below runs against it so the Kueue
/// block never silently changes what they assert.
fn inert_kueue() -> KueueGateOutcome {
    crate::repositories::task::board_health_kueue_admission::kueue_gate(
        &crate::repositories::task::board_health_kueue_admission::KueueProjection::Inert,
        "task-1",
    )
}

fn gate(ledger: &LeaseLedger, task_id: &str) -> serde_json::Value {
    dispatch_gate_json(
        "worker",
        &["task_edit"],
        None,
        true,
        false,
        false,
        false,
        true,
        None,
        None,
        lease_gate(ledger, task_id),
        inert_kueue(),
        Vec::new(),
    )
}

/// While the projection is empty the Kueue gate must be declared UNevaluated
/// and must contribute no reason: an unarmed cluster is not a stalled one.
#[test]
fn an_empty_kueue_projection_leaves_the_gate_unevaluated() {
    let ledger = observed(Vec::new(), capacity(0, 3, true));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert_eq!(
        gate["kueue_admission"]["projection_state"],
        "no_workloads_observed"
    );
    assert!(gate["kueue_workload"].is_null());
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(unevaluated.contains(&serde_json::json!("kueue_clusterqueue_admission")));
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(!evaluated.contains(&serde_json::json!("kueue_clusterqueue_admission")));
}

/// **The gate this reader was written to promote.** With rows in the
/// projection, `kueue_clusterqueue_admission` moves from `unevaluated_gates` to
/// `evaluated_gates` — it is no longer a permanent blind spot.
#[test]
fn an_observing_projection_promotes_the_kueue_gate_to_evaluated() {
    let ledger = observed(Vec::new(), capacity(0, 3, true));
    let kueue = KueueGateOutcome {
        kueue_admission: serde_json::json!({ "projection_state": "observing" }),
        kueue_workload: serde_json::json!({ "admission": "pending" }),
        reasons: vec!["kueue_workload_pending"],
        evaluated: true,
        unevaluated_detail: None,
    };
    let gate = dispatch_gate_json(
        "worker",
        &["task_edit"],
        None,
        true,
        false,
        false,
        false,
        true,
        None,
        None,
        lease_gate(&ledger, "task-1"),
        kueue,
        Vec::new(),
    );
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("kueue_workload_pending"))
    );
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(evaluated.contains(&serde_json::json!("kueue_clusterqueue_admission")));
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(!unevaluated.contains(&serde_json::json!("kueue_clusterqueue_admission")));
}

/// The regression this module exists for: a task denied for a capacity /
/// lease reason must never report an empty-`reasons` verdict again.
#[test]
fn queued_dispatch_lease_is_a_named_capacity_reason() {
    let ledger = observed(vec![("task-1", lease_row("queued"))], capacity(3, 3, true));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert_eq!(
        gate["reasons"].as_array().unwrap(),
        &vec![serde_json::json!("build_lease_queued")]
    );
    assert_eq!(gate["build_lease"]["state"], "queued");
    assert_eq!(gate["build_lease"]["queued_ahead"], 2);
    assert_eq!(gate["build_capacity"]["occupancy"], 3);
    assert_eq!(gate["build_capacity"]["cap"], 3);
    assert_eq!(gate["build_capacity"]["at_capacity"], true);
}

/// The #2661 tombstone shape: the newest attempt is terminal.
#[test]
fn terminal_dispatch_lease_is_reported_as_such() {
    let mut row = lease_row("terminal");
    row.terminal_reason = Some("reclaimed_absent".to_owned());
    let ledger = observed(vec![("task-1", row)], capacity(0, 3, true));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("build_lease_terminal"))
    );
    assert_eq!(gate["build_lease"]["terminal_reason"], "reclaimed_absent");
}

/// A full pool is a real, checkable explanation for a task that never even
/// got a row. This is the signal that was missing while one task
/// monopolised the board.
#[test]
fn full_pool_explains_a_task_with_no_lease_row() {
    let ledger = observed(Vec::new(), capacity(3, 3, true));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert_eq!(
        gate["reasons"].as_array().unwrap(),
        &vec![serde_json::json!("build_pool_at_capacity")]
    );
    assert!(gate["build_lease"].is_null());
}

/// Neutralisation guard: with no lease evidence and a pool that is NOT
/// full, nothing is claimed. The verdict is `unexplained`, never
/// `stranded`, and `stranded` is not emitted anywhere.
#[test]
fn no_evidence_yields_unexplained_not_stranded() {
    let ledger = observed(Vec::new(), capacity(0, 3, true));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert!(gate["reasons"].as_array().unwrap().is_empty());
    assert_ne!(gate["gate_verdict"], "stranded");
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(
        !unevaluated.is_empty(),
        "an empty `reasons` must ship the list of gates it did not consult"
    );
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(evaluated.contains(&serde_json::json!("build_lease_admission")));
}

/// An unreadable ledger must move the lease gate from evaluated to
/// unevaluated rather than pass as a clean one.
#[test]
fn unobservable_ledger_is_declared_unevaluated() {
    let ledger = LeaseLedger::Unobservable { detail: "boom" };
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert!(gate["build_lease"].is_null());
    assert!(gate["build_capacity"].is_null());
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(!evaluated.contains(&serde_json::json!("build_lease_admission")));
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(unevaluated.contains(&serde_json::json!("build_lease_admission")));
}

/// A non-armed v1 authority grants and denies nothing, so it must not be
/// blamed for a full pool it is not enforcing.
#[test]
fn shadow_mode_pool_is_not_blamed() {
    let ledger = observed(Vec::new(), capacity(9, 3, false));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert_eq!(gate["build_capacity"]["lease_authority_enforcing"], false);
    assert_eq!(gate["build_capacity"]["at_capacity"], true);
}

/// A model-health reason still blocks, and the lease gate composes with it.
#[test]
fn model_reasons_and_lease_reasons_compose() {
    let ledger = observed(vec![("task-1", lease_row("queued"))], capacity(3, 3, true));
    let gate = dispatch_gate_json(
        "worker",
        &["task_edit"],
        Some("prov/model".to_owned()),
        false,
        false,
        false,
        false,
        true,
        None,
        None,
        lease_gate(&ledger, "task-1"),
        inert_kueue(),
        vec!["no_eligible_model"],
    );
    let reasons = gate["reasons"].as_array().unwrap();
    assert!(reasons.contains(&serde_json::json!("no_eligible_model")));
    assert!(reasons.contains(&serde_json::json!("build_lease_queued")));
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
}

/// **`build_capacity` must name its own authority.**
///
/// The exact payload from 2026-07-29: `{occupancy: 1, cap: 3, enforcing:
/// true, at_capacity: false}`. Every number was correct. Read by an
/// operator it said "capacity is fine, look elsewhere" — while the
/// build-admission controller was denying every dispatch with
/// `controller_not_admitting` before capacity was ever measured.
///
/// The unqualified key `enforcing` is the specific misreading: it invites
/// "build admission is enforcing and finds room", which is a claim about a
/// different authority. It must not come back.
#[test]
fn build_capacity_cannot_be_mistaken_for_the_admission_authority() {
    let ledger = observed(Vec::new(), capacity(1, 3, true));
    let gate = gate(&ledger, "task-1");
    let capacity_block = &gate["build_capacity"];

    assert_eq!(capacity_block["authority"], "build_leases");
    assert_eq!(capacity_block["lease_authority_enforcing"], true);
    assert!(
        capacity_block.get("enforcing").is_none(),
        "the unqualified `enforcing` key is what was misread for five hours"
    );
    let note = capacity_block["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("LEASE authority only"),
        "the payload must state its own bound: {note}"
    );
    assert!(
        note.contains("build_admission_denial"),
        "the note must point at the dispatcher's own recorded decision: {note}"
    );
}

// ── Persisted denial cause (#2661) ──────────────────────────────────────

/// **The whole point of persisting the cause.** The dispatcher knew, on
/// every tick for five hours, that it was refusing this task and exactly
/// why. It logged it and threw it away, and this section reported
/// `unexplained` with an empty `reasons` the entire time.
#[test]
fn a_recorded_denial_is_the_dispatchers_own_reason() {
    let mut denial = denial_row(
        "controller_not_admitting",
        Some("create_unknown_health"),
        30,
    );
    // Legacy decision measurements can differ from the report-time global
    // ledger snapshot and must never become the rendered global capacity.
    denial.detail = Some("controller is warming".to_owned());
    denial.occupancy = Some(0);
    denial.cap = 0;
    let ledger = observed_full(
        Vec::new(),
        capacity(7, 11, true),
        denials(vec![("task-1", denial)]),
    );
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "build_admission_denied_controller_not_admitting"
            )),
        "reasons: {:?}",
        gate["reasons"]
    );
    let denial = &gate["build_admission_denial"];
    assert_eq!(denial["scope"], "global");
    assert_eq!(denial["authority"], "build_leases");
    assert_eq!(denial["occupancy"], 7);
    assert_eq!(denial["cap"], 11);
    // Persisted decision evidence remains intact even though legacy capacity
    // measurements are deliberately not rendered.
    assert_eq!(denial["cause"], "controller_not_admitting");
    // The `readiness=` field that lived only in container logs.
    assert_eq!(denial["readiness"], "create_unknown_health");
    assert_eq!(denial["detail"], "controller is warming");
    assert_eq!(denial["server_epoch"], "epoch-1");
    assert_eq!(denial["first_denied_at"], "2026-07-29T06:22:57.000Z");
    assert_eq!(denial["denied_at"], "2026-07-29T11:30:00.000Z");
    assert_eq!(denial["denial_count"], 152);
    assert_eq!(denial["fresh"], true);
}

/// Each cause maps to its own reason, so `at_capacity` and
/// `controller_not_admitting` can never again be read as the same thing.
#[test]
fn each_denial_cause_maps_to_a_distinct_reason() {
    for (cause, expected) in [
        ("at_capacity", "build_admission_denied_at_capacity"),
        (
            "controller_not_admitting",
            "build_admission_denied_controller_not_admitting",
        ),
        (
            "authority_unavailable",
            "build_admission_denied_authority_unavailable",
        ),
    ] {
        let ledger = observed_full(
            Vec::new(),
            capacity(0, 3, true),
            denials(vec![("task-1", denial_row(cause, None, 10))]),
        );
        let gate = gate(&ledger, "task-1");
        assert!(
            gate["reasons"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(expected)),
            "`{cause}` must produce `{expected}`, got {:?}",
            gate["reasons"]
        );
    }
}

/// **Neutralisation guard — the #2661 failure mode, in a new table.**
///
/// #2661 was a tombstone bug: a spent row was replayed forever and every
/// denial re-derived itself. A stale denial record must NOT be blamed. It
/// is still reported, with `fresh: false` and its age, because "the last
/// thing that happened was this, a while ago" is real evidence — it is
/// just not a current reason.
#[test]
fn a_stale_denial_record_is_reported_but_never_blamed() {
    let ledger = observed_full(
        Vec::new(),
        capacity(0, 3, true),
        denials(vec![(
            "task-1",
            denial_row(
                "controller_not_admitting",
                Some("create_unknown_health"),
                DENIAL_FRESHNESS_SECONDS + 1,
            ),
        )]),
    );
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert!(gate["reasons"].as_array().unwrap().is_empty());
    assert_eq!(gate["build_admission_denial"]["fresh"], false);
    assert_eq!(
        gate["build_admission_denial"]["cause"],
        "controller_not_admitting"
    );
}

/// The permitted path DELETES the row, so its absence is a real answer and
/// the gate counts as evaluated.
#[test]
fn no_recorded_denial_is_an_evaluated_answer_not_a_gap() {
    let ledger = observed(Vec::new(), capacity(0, 3, true));
    let gate = gate(&ledger, "task-1");
    assert!(gate["build_admission_denial"].is_null());
    assert!(
        gate["coverage"]["evaluated_gates"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("build_admission_denial"))
    );
}

/// An unreadable denial table must be declared, never read as "nobody was
/// denied".
#[test]
fn unobservable_denials_are_declared_unevaluated() {
    let ledger = observed_full(Vec::new(), capacity(0, 3, true), None);
    let gate = gate(&ledger, "task-1");
    assert!(gate["build_admission_denial"].is_null());
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(!evaluated.contains(&serde_json::json!("build_admission_denial")));
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(unevaluated.contains(&serde_json::json!("build_admission_denial")));
    assert!(
        gate["coverage"]["build_admission_denial_unevaluated_detail"]
            .as_str()
            .is_some()
    );
}

/// Another task's denial is not this task's reason.
#[test]
fn a_denial_recorded_for_another_task_is_not_borrowed() {
    let ledger = observed_full(
        Vec::new(),
        capacity(0, 3, true),
        denials(vec![("task-2", denial_row("at_capacity", None, 5))]),
    );
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert!(gate["build_admission_denial"].is_null());
}

// ── Strand clock ────────────────────────────────────────────────────────

/// The `bx1f` case: created long ago, never dispatched (so no open
/// transition and no session release), blocked until its blocker merged.
/// The clock must start when the blocker cleared, not at creation.
#[test]
fn blocker_clearing_resets_the_strand_clock() {
    let (at, confidence, basis) = strand_clock(
        "2026-07-26T10:00:00.000Z",
        None,
        None,
        Some("2026-07-27T04:24:00.000Z"),
        Some("2026-07-27T04:24:30.000Z"),
    );
    assert_eq!(at, "2026-07-27T04:24:00.000Z");
    assert_eq!(confidence, "high");
    assert_eq!(basis, "blocker_cleared");
}

/// With no recorded close event the blocking task's `updated_at` is the
/// proxy, and it is reported as the low-confidence signal it is.
#[test]
fn blocker_updated_at_is_the_low_confidence_proxy() {
    let (at, confidence, basis) = strand_clock(
        "2026-07-26T10:00:00.000Z",
        None,
        None,
        None,
        Some("2026-07-27T04:24:00.000Z"),
    );
    assert_eq!(at, "2026-07-27T04:24:00.000Z");
    assert_eq!(confidence, "low");
    assert_eq!(basis, "blocker_task_updated_at");
}

/// A blocker that cleared BEFORE the task became open must not drag the
/// clock backwards.
#[test]
fn an_older_blocker_clear_does_not_win() {
    let (_, _, basis) = strand_clock(
        "2026-07-26T10:00:00.000Z",
        Some("2026-07-27T09:00:00.000Z"),
        None,
        Some("2026-07-27T04:24:00.000Z"),
        None,
    );
    assert_eq!(basis, "open_transition");
}

/// **Neutralisation guard for the clock change.** `tasks.updated_at` is
/// bumped by any write — a description edit, a label change — so it must
/// never participate in the `max`. If it did, editing a starved task would
/// silently reset its strand clock and hide exactly the starvation this
/// section exists to find.
#[test]
fn a_recent_task_edit_cannot_reset_the_clock() {
    let (at, confidence, basis) = strand_clock(
        // Edited seconds ago...
        "2026-07-27T12:00:00.000Z",
        // ...but dispatchable since last year.
        Some("2025-01-01T00:00:00.000Z"),
        None,
        None,
        None,
    );
    assert_eq!(at, "2025-01-01T00:00:00.000Z");
    assert_eq!(confidence, "high");
    assert_eq!(basis, "open_transition");
}

/// With no became-dispatchable signal at all the fallback is used, and the
/// basis says so, so the reported number can be discounted.
#[test]
fn fallback_reports_low_confidence_and_its_basis() {
    let (at, confidence, basis) = strand_clock("2026-07-27T09:00:00.000Z", None, None, None, None);
    assert_eq!(at, "2026-07-27T09:00:00.000Z");
    assert_eq!(confidence, "low");
    assert_eq!(basis, "task_updated_at");
}

/// Identical timestamps: the recorded event wins over the fallback.
#[test]
fn ties_prefer_the_recorded_event() {
    let resolved = resolve_unclaimed_since(&[
        StrandSignal {
            at: "2026-07-27T09:00:00.000Z",
            confidence: "low",
            basis: "task_updated_at",
        },
        StrandSignal {
            at: "2026-07-27T09:00:00.000Z",
            confidence: "high",
            basis: "session_release",
        },
    ])
    .expect("a signal is always available");
    assert_eq!(resolved.2, "session_release");
}
