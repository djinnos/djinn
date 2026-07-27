//! Dispatch-gate evidence for the stranded-ready board-health section.
//!
//! # Why this module exists
//!
//! The stranded-ready section used to emit, per stranded task:
//!
//! ```json
//! "dispatch_gate": { "image_ready": true, "breaker_open": false, ...,
//!                    "gate_verdict": "stranded", "reasons": [] }
//! ```
//!
//! Every field in that payload was true. It was also irrelevant. `reasons`
//! could only ever be populated from `dispatch_state.inflight_model_id` x
//! `model_health`, so a task with no chosen model produced a **structurally
//! guaranteed** empty `reasons` and a **structurally guaranteed** `stranded`
//! verdict. The section performed an independent re-evaluation over six gates
//! while the real dispatcher applies more than thirty, and it never read the
//! dispatcher's own recorded decision. On 2026-07-27 six tasks sat undispatched
//! for up to eighteen hours behind a build-lease tombstone, and this payload
//! asserted, once every thirty seconds, that nothing was wrong with any of them.
//!
//! An empty `reasons` list that actually means *"I did not look"* is worse than
//! no field at all. Two rules follow, and both are enforced here:
//!
//! 1. **No verdict this code cannot justify.** `stranded` is gone. An empty
//!    `reasons` now yields `unexplained` — literally "none of the gates this
//!    section can evaluate explains the non-dispatch" — and the payload names
//!    which gates were and were not consulted so the claim is auditable.
//! 2. **Read what the dispatcher actually recorded.** Layer-1 build admission
//!    persists its unit of work in `build_leases` as a `task_dispatch` row. A
//!    queued row means this task is behind the FIFO, a terminal row is the
//!    #2661 tombstone shape, and ledger-wide `SUM(weight)` versus
//!    `build_lease_caps.cap` is the pool occupancy the dispatcher itself
//!    consults. That is durable, joinable evidence and it is now surfaced.
//!
//! # What is NOT available
//!
//! `BuildAdmissionDecision::Denied` carries a [`DenialCause`] as of #2661, but
//! that value is only ever **logged** — `dispatch/task_dispatch.rs` emits it on
//! a `tracing::info!` line and returns. Nothing writes it to any table, so
//! `ControllerNotAdmitting` and `AuthorityUnavailable { detail }` cannot be
//! joined against from here at all, and `AtCapacity` is only *inferable* from
//! the ledger state this module reads. Persisting the cause on the denial path
//! is tracked as follow-up; until then this module reports ledger facts and
//! declines to invent the rest.
//!
//! [`DenialCause`]: https://github.com/djinnos/djinn/pull/2661

use std::collections::HashMap;

use sqlx::Row;

/// Gates the stranded-ready section evaluates from durable state.
///
/// This list is the honest scope of `reasons`: an empty `reasons` means none of
/// **these** fired, and says nothing whatsoever about the rest of the dispatch
/// path.
pub(super) const EVALUATED_GATES: &[&str] = &[
    "breaker_cooldown",
    "rate_limit_backoff",
    "manual_dispatch_pause",
    "owner_credential",
    "model_health",
    "build_lease_admission",
];

/// Gates the real dispatcher applies that this section cannot see.
///
/// Not exhaustive and deliberately not silent: every entry here is a way a task
/// can be left queued while this section reports `unexplained`. Sourced from
/// the per-task gate sequence in `djinn-coordinator`'s `dispatch/task_dispatch.rs`.
pub(super) const UNEVALUATED_GATES: &[&str] = &[
    "per_user_lane_concurrency_cap",
    "per_user_model_concurrency_cap",
    "slot_pool_capacity",
    "provider_circuit_breaker",
    "failover_chain_exhaustion",
    "respawn_guard_attempt_defer",
    "project_dispatch_image_readiness",
    "arbiter_deadline_hold",
    "human_review_hold_label",
    "creator_attribution",
    "github_app_credentials",
    "legacy_settings_migration_hold",
];

/// Lease states that occupy build capacity. Mirrors the partial index in
/// migration 153 (`build_leases_occupied_idx`).
const OCCUPYING_LEASE_STATES: &[&str] = &["granted", "launching", "bound", "active", "suspect"];

/// One `task_dispatch` build-lease row: the dispatcher's own record of a
/// layer-1 admission attempt for a task.
#[derive(Clone, Debug)]
pub(super) struct DispatchLeaseRow {
    pub consumer_id: String,
    pub state: String,
    pub terminal_reason: Option<String>,
    pub weight: i64,
    pub enqueue_sequence: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Queued rows ahead of this one in the FIFO. Only meaningful while
    /// `state == "queued"`.
    pub queued_ahead: i64,
}

/// Ledger-wide capacity as the dispatcher's authority reads it.
#[derive(Clone, Debug)]
pub(super) struct LeaseCapacity {
    pub occupancy: i64,
    pub cap: i64,
    /// True only when the durable admission epoch has armed the v1 authority
    /// (`admission_handoff.v1_mode = 'enforce'`). While it is `off`/`shadow`
    /// the lease FIFO writes no dispatch rows and cannot be denying anything.
    pub enforcing: bool,
}

/// What this section managed to learn about the build-lease ledger.
///
/// `Unobservable` is a first-class outcome on purpose. Falling back to "no
/// rows" on a failed read is exactly the mistake this module exists to undo: it
/// would let an unread ledger masquerade as a clean one.
#[derive(Clone, Debug)]
pub(super) enum LeaseLedger {
    Observed {
        by_task: HashMap<String, DispatchLeaseRow>,
        capacity: LeaseCapacity,
    },
    Unobservable {
        detail: &'static str,
    },
}

/// Result of applying the build-lease gate to one task.
pub(super) struct LeaseGateOutcome {
    /// The task's own lease row as JSON, or `null` when it has none.
    pub build_lease: serde_json::Value,
    /// Pool-wide capacity as JSON, or `null` when the ledger was unreadable.
    pub build_capacity: serde_json::Value,
    /// Machine-readable reasons contributed by this gate.
    pub reasons: Vec<&'static str>,
    /// False when the ledger could not be read, which moves
    /// `build_lease_admission` from evaluated to unevaluated for this task.
    pub evaluated: bool,
    /// Why the gate could not be evaluated, surfaced in `coverage` so an
    /// unreadable ledger is visible rather than merely absent.
    pub unevaluated_detail: Option<&'static str>,
}

/// Load the `task_dispatch` lease ledger and the pool capacity in two queries.
///
/// Both are runtime `sqlx::query` calls (no offline-cache entries), matching
/// the rest of the board-health sections. Either failing yields
/// [`LeaseLedger::Unobservable`] — never a silently empty ledger.
pub(super) async fn load_lease_ledger(pool: &sqlx::PgPool) -> LeaseLedger {
    // Newest attempt per task. `consumer_id` is `{task_id}:{generation}` (see
    // `djinn-coordinator`'s `build_lease::identity`), and task ids never
    // contain a colon, so `split_part(..., ':', 1)` is an exact task id.
    let lease_sql = r#"SELECT DISTINCT ON (split_part(b.consumer_id, ':', 1))
                  split_part(b.consumer_id, ':', 1) AS task_id,
                  b.consumer_id,
                  b.state,
                  b.terminal_reason,
                  b.weight::BIGINT AS weight,
                  b.enqueue_sequence::BIGINT AS enqueue_sequence,
                  to_char(b.created_at AT TIME ZONE 'utc',
                          'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS created_at,
                  to_char(b.updated_at AT TIME ZONE 'utc',
                          'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS updated_at,
                  (SELECT COUNT(*) FROM build_leases q
                    WHERE q.state = 'queued'
                      AND q.enqueue_sequence < b.enqueue_sequence)::BIGINT AS queued_ahead
           FROM build_leases b
           WHERE b.consumer_kind = 'task_dispatch'
           ORDER BY split_part(b.consumer_id, ':', 1), b.enqueue_sequence DESC"#;

    let Ok(rows) = sqlx::query(lease_sql).fetch_all(pool).await else {
        return LeaseLedger::Unobservable {
            detail: "build_leases could not be read",
        };
    };

    // Occupancy is the weighted SUM over the occupying states across EVERY
    // consumer kind — warm and invocation leases contend with dispatch for the
    // same cap. `admission_handoff.v1_mode` says whether that cap is armed.
    let capacity_sql = r#"SELECT
             COALESCE((SELECT SUM(weight) FROM build_leases
                        WHERE state IN ('granted','launching','bound','active','suspect')),
                      0)::BIGINT AS occupancy,
             COALESCE((SELECT cap FROM build_lease_caps WHERE singleton), 0)::BIGINT AS cap,
             COALESCE((SELECT v1_mode FROM admission_handoff WHERE name = 'build'),
                      'off') AS v1_mode"#;

    let Ok(capacity_row) = sqlx::query(capacity_sql).fetch_one(pool).await else {
        return LeaseLedger::Unobservable {
            detail: "build-lease capacity could not be read",
        };
    };

    let capacity = LeaseCapacity {
        occupancy: capacity_row.try_get("occupancy").unwrap_or(0),
        cap: capacity_row.try_get("cap").unwrap_or(0),
        enforcing: capacity_row
            .try_get::<String, _>("v1_mode")
            .map(|mode| mode == "enforce")
            .unwrap_or(false),
    };

    let by_task = rows
        .into_iter()
        .filter_map(|row| {
            let task_id: String = row.try_get("task_id").ok()?;
            Some((
                task_id,
                DispatchLeaseRow {
                    consumer_id: row.try_get("consumer_id").ok()?,
                    state: row.try_get("state").ok()?,
                    terminal_reason: row.try_get("terminal_reason").ok().flatten(),
                    weight: row.try_get("weight").unwrap_or(0),
                    enqueue_sequence: row.try_get("enqueue_sequence").unwrap_or(0),
                    created_at: row.try_get("created_at").ok().flatten(),
                    updated_at: row.try_get("updated_at").ok().flatten(),
                    queued_ahead: row.try_get("queued_ahead").unwrap_or(0),
                },
            ))
        })
        .collect();

    LeaseLedger::Observed { by_task, capacity }
}

/// Apply the build-lease gate to one task.
///
/// Every reason produced here is a statement about a durable row, never an
/// inference about what the dispatcher "probably" did.
pub(super) fn lease_gate(ledger: &LeaseLedger, task_id: &str) -> LeaseGateOutcome {
    let (by_task, capacity) = match ledger {
        LeaseLedger::Unobservable { detail } => {
            return LeaseGateOutcome {
                build_lease: serde_json::Value::Null,
                build_capacity: serde_json::Value::Null,
                reasons: Vec::new(),
                evaluated: false,
                unevaluated_detail: Some(detail),
            };
        }
        LeaseLedger::Observed { by_task, capacity } => (by_task, capacity),
    };

    let at_capacity = capacity.cap > 0 && capacity.occupancy >= capacity.cap;
    let build_capacity = serde_json::json!({
        "occupancy": capacity.occupancy,
        "cap":       capacity.cap,
        "enforcing": capacity.enforcing,
        "at_capacity": at_capacity,
    });

    let lease = by_task.get(task_id);
    let build_lease = lease.map_or(serde_json::Value::Null, |row| {
        serde_json::json!({
            "consumer_id":      row.consumer_id,
            "state":            row.state,
            "terminal_reason":  row.terminal_reason,
            "weight":           row.weight,
            "enqueue_sequence": row.enqueue_sequence,
            "queued_ahead":     row.queued_ahead,
            "created_at":       row.created_at,
            "updated_at":       row.updated_at,
        })
    });

    // While the v1 authority is off or in shadow the FIFO writes no dispatch
    // rows and grants nothing, so it is structurally incapable of denying this
    // task. Reporting a capacity reason here would be the same fabrication this
    // module exists to remove — but the gate WAS evaluated, and the answer is
    // "not this".
    if !capacity.enforcing {
        return LeaseGateOutcome {
            build_lease,
            build_capacity,
            reasons: Vec::new(),
            evaluated: true,
            unevaluated_detail: None,
        };
    }

    let mut reasons: Vec<&'static str> = Vec::new();
    match lease {
        // The task holds a FIFO position: layer-1 admission denied it and the
        // ledger says why — the pool was full when it asked.
        Some(row) if row.state == "queued" => reasons.push("build_lease_queued"),
        // The newest attempt is terminal. This is the #2661 tombstone shape:
        // before that fix a spent row was replayed forever and every denial
        // re-derived itself.
        Some(row) if row.state == "terminal" => reasons.push("build_lease_terminal"),
        // The lease occupies capacity but this task has no running session (the
        // section already excluded tasks that do). The slot is charged to work
        // that is not happening.
        Some(row) if OCCUPYING_LEASE_STATES.contains(&row.state.as_str()) => {
            reasons.push("build_lease_occupied_without_session");
        }
        Some(_) => {}
        // No row of its own, but the shared pool is full: the next admission
        // attempt for this task cannot be granted. This is the signal that was
        // missing while a monopolising task starved the board.
        None if at_capacity => reasons.push("build_pool_at_capacity"),
        None => {}
    }

    LeaseGateOutcome {
        build_lease,
        build_capacity,
        reasons,
        evaluated: true,
        unevaluated_detail: None,
    }
}

// ── Strand clock ────────────────────────────────────────────────────────────

/// One candidate answer to "since when has this task been dispatchable".
#[derive(Clone, Copy, Debug)]
pub(super) struct StrandSignal<'a> {
    pub at: &'a str,
    /// `high` for a recorded event, `low` for a `updated_at`-shaped fallback.
    pub confidence: &'static str,
    /// Which signal this is, so the number can be argued with.
    pub basis: &'static str,
}

/// Resolve the strand clock from *became-dispatchable* signals: the LATEST one
/// wins.
///
/// Latest, not first-available, because every signal here marks a moment the
/// task became dispatchable and only the most recent one is still true.
///
/// Timestamps are the fixed-width `YYYY-MM-DDTHH:MM:SS.MSZ` form Postgres
/// `to_char` produces throughout this schema, so lexicographic ordering is
/// chronological ordering. Ties prefer the higher-confidence basis.
pub(super) fn resolve_unclaimed_since(
    candidates: &[StrandSignal<'_>],
) -> Option<(String, &'static str, &'static str)> {
    candidates
        .iter()
        .max_by(|a, b| {
            a.at.cmp(b.at)
                .then_with(|| confidence_rank(a.confidence).cmp(&confidence_rank(b.confidence)))
        })
        .map(|winner| (winner.at.to_owned(), winner.confidence, winner.basis))
}

fn confidence_rank(confidence: &str) -> u8 {
    match confidence {
        "high" => 2,
        _ => 1,
    }
}

/// The strand clock for one stranded-ready row.
///
/// # Two kinds of timestamp, and why they are not interchangeable
///
/// `open_transition_at`, `session_release_at` and the blocker-clear pair each
/// mark a moment the task **became dispatchable**. The latest of them is the
/// answer, because it is the only one still true. Adding the blocker-clear
/// signal is the fix for a task created at 10:00 and blocked until 04:24 the
/// next day: it was not stranded for eighteen hours, it was blocked for ten of
/// them, and the section excluded it the whole time without ever restarting its
/// clock.
///
/// `task_updated_at` is **not** such a moment. It is bumped by any write to the
/// task — a description edit, a label change — so folding it into the same
/// `max` would silently reset the clock on a genuinely starved task and hide
/// the starvation this section exists to find. It is used only when no
/// became-dispatchable signal exists at all, and is reported as `low`
/// confidence when it is.
pub(super) fn strand_clock(
    task_updated_at: &str,
    open_transition_at: Option<&str>,
    session_release_at: Option<&str>,
    blocker_closed_event_at: Option<&str>,
    blocker_task_updated_at: Option<&str>,
) -> (String, &'static str, &'static str) {
    let mut signals: Vec<StrandSignal<'_>> = Vec::new();
    if let Some(at) = open_transition_at {
        signals.push(StrandSignal {
            at,
            confidence: "high",
            basis: "open_transition",
        });
    }
    if let Some(at) = session_release_at {
        signals.push(StrandSignal {
            at,
            confidence: "high",
            basis: "session_release",
        });
    }
    match (blocker_closed_event_at, blocker_task_updated_at) {
        (Some(at), _) => signals.push(StrandSignal {
            at,
            confidence: "high",
            basis: "blocker_cleared",
        }),
        // No recorded close event for the blocker; its `updated_at` is the best
        // available proxy and is flagged as such.
        (None, Some(at)) => signals.push(StrandSignal {
            at,
            confidence: "low",
            basis: "blocker_task_updated_at",
        }),
        (None, None) => {}
    }
    resolve_unclaimed_since(&signals)
        .unwrap_or_else(|| (task_updated_at.to_owned(), "low", "task_updated_at"))
}

// ── Verdict ─────────────────────────────────────────────────────────────────

/// The verdict emitted when a gate this section CAN evaluate fired.
pub(super) const VERDICT_BLOCKED: &str = "blocked";
/// The verdict emitted when none did.
///
/// Deliberately not `stranded`. The section cannot see most of the dispatch
/// path, so "no reason found" is a statement about this code's coverage, not
/// about the task.
pub(super) const VERDICT_UNEXPLAINED: &str = "unexplained";

/// Assemble the `dispatch_gate` payload.
///
/// `reasons` and `gate_verdict` are derived together from the same list so they
/// can never disagree, and `coverage` travels with them so an empty `reasons`
/// can be read as the bounded claim it is.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_gate_json(
    evaluated_role: &str,
    toolset: &'static [&'static str],
    model_requirement: Option<String>,
    image_ready: bool,
    breaker_open: bool,
    manually_paused: bool,
    rate_limited: bool,
    credential_available: bool,
    last_dispatched_role: Option<String>,
    cooldown_until: Option<String>,
    lease: LeaseGateOutcome,
    mut reasons: Vec<&'static str>,
) -> serde_json::Value {
    reasons.extend(lease.reasons);

    let mut evaluated_gates: Vec<&'static str> = EVALUATED_GATES
        .iter()
        .copied()
        .filter(|gate| lease.evaluated || *gate != "build_lease_admission")
        .collect();
    evaluated_gates.sort_unstable();

    let mut unevaluated_gates: Vec<&'static str> = UNEVALUATED_GATES.to_vec();
    if !lease.evaluated {
        unevaluated_gates.push("build_lease_admission");
    }
    unevaluated_gates.sort_unstable();

    let gate_verdict = if reasons.is_empty() {
        VERDICT_UNEXPLAINED
    } else {
        VERDICT_BLOCKED
    };

    serde_json::json!({
        "evaluated_role":       evaluated_role,
        "toolset":              toolset,
        "model_requirement":    model_requirement,
        "image_ready":          image_ready,
        "breaker_open":         breaker_open,
        "manually_paused":      manually_paused,
        "rate_limited":         rate_limited,
        "credential_available": credential_available,
        "build_lease":          lease.build_lease,
        "build_capacity":       lease.build_capacity,
        "gate_verdict":         gate_verdict,
        "reasons":              reasons,
        "coverage": serde_json::json!({
            "scope":             "partial",
            "evaluated_gates":   evaluated_gates,
            "unevaluated_gates": unevaluated_gates,
            "build_lease_unevaluated_detail": lease.unevaluated_detail,
            "note": "`reasons` covers only `evaluated_gates`. An empty `reasons` \
                     yields `unexplained`, which means no evaluated gate fired — \
                     NOT that the dispatcher had no reason. The dispatcher's own \
                     DenialCause (#2661) is logged, never persisted, so it cannot \
                     be joined here.",
        }),
        // Retained for backward compatibility with the initial board_health
        // contract.
        "last_dispatched_role": last_dispatched_role,
        "cooldown_until":       cooldown_until,
    })
}

#[cfg(test)]
mod tests {
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
        LeaseLedger::Observed {
            by_task: rows
                .into_iter()
                .map(|(id, row)| (id.to_owned(), row))
                .collect(),
            capacity,
        }
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
        assert_eq!(gate["build_capacity"]["enforcing"], false);
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
}
