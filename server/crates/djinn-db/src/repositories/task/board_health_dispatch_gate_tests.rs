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
    observed_with_journal(
        rows,
        capacity,
        JournalSignal::Observed(AdmissionJournalSignal::default()),
    )
}

fn observed_with_journal(
    rows: Vec<(&str, DispatchLeaseRow)>,
    capacity: LeaseCapacity,
    journal: JournalSignal,
) -> LeaseLedger {
    observed_full(rows, capacity, journal, Some(HashMap::new()))
}

fn observed_full(
    rows: Vec<(&str, DispatchLeaseRow)>,
    capacity: LeaseCapacity,
    journal: JournalSignal,
    denials: Option<HashMap<String, DenialRow>>,
) -> LeaseLedger {
    LeaseLedger::Observed {
        by_task: rows
            .into_iter()
            .map(|(id, row)| (id.to_owned(), row))
            .collect(),
        capacity,
        journal,
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

fn journal(active: i64, settled: i64) -> JournalSignal {
    JournalSignal::Observed(AdmissionJournalSignal {
        create_unknown_active: active,
        create_unknown_settled: settled,
        distinct_creator_epochs: if active > 0 { 1 } else { 0 },
        oldest_create_unknown_at: (active > 0).then(|| "2026-07-29T06:22:57.000Z".to_owned()),
    })
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
        Vec::new(),
    )
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
        vec!["no_eligible_model"],
    );
    let reasons = gate["reasons"].as_array().unwrap();
    assert!(reasons.contains(&serde_json::json!("no_eligible_model")));
    assert!(reasons.contains(&serde_json::json!("build_lease_queued")));
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
}

// ── Build-admission journal authority ───────────────────────────────────

/// **The 2026-07-29 outage, reproduced.** A settled `create_unknown` row
/// held `CreateUnknownHealth` and every dispatch was denied with
/// `controller_not_admitting`, while the lease authority reported
/// `occupancy: 1, cap: 3, at_capacity: false`. This section reported
/// `unexplained` with `reasons: []` for five hours.
///
/// Before this gate existed the assertions below were unreachable: the
/// only surface naming the cause was a `readiness=` field in a container
/// log on the node.
#[test]
fn a_settled_create_unknown_row_names_the_outage() {
    let ledger = observed_with_journal(Vec::new(), capacity(1, 3, true), journal(1, 1));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("admission_create_unknown_pending")),
        "reasons must name the journal authority: {:?}",
        gate["reasons"]
    );
    // The lease authority is simultaneously healthy. Both facts are true;
    // only one of them explains the strand.
    assert_eq!(gate["build_capacity"]["at_capacity"], false);
    assert_eq!(gate["build_admission"]["create_unknown_settled"], 1);
    assert_eq!(gate["build_admission"]["authority"], "admission_journal");
    assert_eq!(
        gate["build_admission"]["settle_window_seconds"],
        RECLAIM_SETTLE_WINDOW_SECONDS
    );
}

/// **Neutralisation guard.** Every healthy task-run passes through
/// `create_unknown` between the pool accepting the create and the
/// `("session","started")` callback. Firing on the ACTIVE count would emit
/// a denial reason during ordinary dispatch — the fabricated-reason bug
/// this module exists to remove. Only the settled count may fire.
#[test]
fn an_in_flight_create_unknown_is_not_a_reason() {
    let ledger = observed_with_journal(Vec::new(), capacity(1, 3, true), journal(3, 0));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["gate_verdict"], VERDICT_UNEXPLAINED);
    assert!(gate["reasons"].as_array().unwrap().is_empty());
    // ...but the population is still REPORTED, so an operator can see it.
    assert_eq!(gate["build_admission"]["create_unknown_active"], 3);
    assert_eq!(gate["build_admission"]["create_unknown_settled"], 0);
}

/// The journal authority denies independently of the lease authority's
/// arming. A board with `v1_mode` off or in shadow was still wedged on
/// 2026-07-29, so the reason must survive the not-enforcing early return.
#[test]
fn journal_reason_survives_a_non_enforcing_lease_authority() {
    let ledger = observed_with_journal(Vec::new(), capacity(0, 3, false), journal(1, 1));
    let gate = gate(&ledger, "task-1");
    assert_eq!(gate["build_capacity"]["lease_authority_enforcing"], false);
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
    assert!(
        gate["reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("admission_create_unknown_pending"))
    );
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
    let ledger = observed_with_journal(Vec::new(), capacity(1, 3, true), journal(1, 1));
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
        note.contains("build_admission"),
        "the note must point at the other authority: {note}"
    );
    // The two authorities disagree in exactly the outage's shape, and the
    // payload now carries both answers side by side.
    assert_eq!(capacity_block["at_capacity"], false);
    assert_eq!(gate["build_admission"]["create_unknown_settled"], 1);
    assert_eq!(gate["gate_verdict"], VERDICT_BLOCKED);
}

/// An unreadable `admission_journal` must be declared, not silently read
/// as a clean one — and it must not take the lease gate down with it.
#[test]
fn unobservable_journal_is_declared_unevaluated() {
    let ledger = observed_with_journal(
        Vec::new(),
        capacity(0, 3, true),
        JournalSignal::Unobservable { detail: "boom" },
    );
    let gate = gate(&ledger, "task-1");
    assert!(gate["build_admission"].is_null());
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    assert!(!evaluated.contains(&serde_json::json!("build_admission_create_unknown")));
    assert!(evaluated.contains(&serde_json::json!("build_lease_admission")));
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();
    assert!(unevaluated.contains(&serde_json::json!("build_admission_create_unknown")));
    assert_eq!(
        gate["coverage"]["build_admission_unevaluated_detail"],
        "boom"
    );
}

/// **The coverage hole this change closes.** `UNEVALUATED_GATES` named
/// twelve dispatcher gates and zero controller-readiness gates, so an
/// operator reading `unexplained` had no way to learn that an entire
/// fail-closed ladder existed above them. Every `BuildAdmissionReadiness`
/// variant must now be accounted for: `create_unknown_health` as an
/// evaluated gate, the rest as declared-unevaluated ones.
#[test]
fn every_readiness_state_is_accounted_for_in_coverage() {
    let ledger = observed(Vec::new(), capacity(0, 3, true));
    let gate = gate(&ledger, "task-1");
    let evaluated = gate["coverage"]["evaluated_gates"].as_array().unwrap();
    let unevaluated = gate["coverage"]["unevaluated_gates"].as_array().unwrap();

    assert!(evaluated.contains(&serde_json::json!("build_admission_create_unknown")));
    // `Healthy` is the absence of a gate, so it has no entry.
    for state in [
        "build_admission_shutdown_draining",
        "build_admission_journal_recovery_incomplete",
        "build_admission_journal_unhealthy",
        "build_admission_seeded_occupancy_above_cap",
        "build_admission_inventory_pending",
        "build_admission_topology_pending",
    ] {
        assert!(
            unevaluated.contains(&serde_json::json!(state)),
            "readiness state `{state}` is not declared in coverage"
        );
    }
}

// ── Persisted denial cause (#2661) ──────────────────────────────────────

/// **The whole point of persisting the cause.** The dispatcher knew, on
/// every tick for five hours, that it was refusing this task and exactly
/// why. It logged it and threw it away, and this section reported
/// `unexplained` with an empty `reasons` the entire time.
#[test]
fn a_recorded_denial_is_the_dispatchers_own_reason() {
    let ledger = observed_full(
        Vec::new(),
        capacity(1, 3, true),
        JournalSignal::Observed(AdmissionJournalSignal::default()),
        denials(vec![(
            "task-1",
            denial_row(
                "controller_not_admitting",
                Some("create_unknown_health"),
                30,
            ),
        )]),
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
    // The `readiness=` field that lived only in container logs.
    assert_eq!(
        gate["build_admission_denial"]["readiness"],
        "create_unknown_health"
    );
    assert_eq!(gate["build_admission_denial"]["denial_count"], 152);
    assert_eq!(gate["build_admission_denial"]["fresh"], true);
    // A readiness denial measures nothing. `null`, never 0.
    assert!(gate["build_admission_denial"]["occupancy"].is_null());
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
            JournalSignal::Observed(AdmissionJournalSignal::default()),
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
        JournalSignal::Observed(AdmissionJournalSignal::default()),
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
    let ledger = observed_full(
        Vec::new(),
        capacity(0, 3, true),
        JournalSignal::Observed(AdmissionJournalSignal::default()),
        None,
    );
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
        JournalSignal::Observed(AdmissionJournalSignal::default()),
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
    let (at, confidence, basis) =
        strand_clock("2026-07-27T09:00:00.000Z", None, None, None, None);
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
