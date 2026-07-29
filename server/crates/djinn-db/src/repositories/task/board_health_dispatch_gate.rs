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
//! # Two authorities, and why both must be read
//!
//! A build dispatch passes through **two independent gates** that this section
//! must not conflate:
//!
//! * the **lease authority** (`build_leases` + `build_lease_caps`, armed by
//!   `admission_handoff.v1_mode`), which is the weighted FIFO and the only one
//!   that ever measures occupancy; and
//! * the **journal authority** (`admission_journal`), owned by the
//!   `BuildAdmissionController`, which fails **closed on a readiness gate
//!   before any capacity is measured at all**.
//!
//! On 2026-07-29 the board stopped dispatching for five hours on the second
//! one: an `admission_journal` row stuck in `create_unknown` armed
//! `CreateUnknownHealth`, and `admit()` returned
//! `Denied { occupancy: None, cause: ControllerNotAdmitting }` for every task.
//! This section read only the lease authority, so it reported a healthy pool
//! (`occupancy: 1, cap: 3, at_capacity: false`) and `gate_verdict:
//! "unexplained"` with `reasons: []` — once every thirty seconds, for five
//! hours, about a board that was completely wedged. The only surface that
//! named the cause was a `readiness=` field in a container log.
//!
//! The journal authority is now read here too. What it can and cannot prove is
//! spelled out on [`AdmissionJournalSignal`].
//!
//! # The dispatcher's own recorded reason
//!
//! `BuildAdmissionDecision::Denied` has carried a `DenialCause` since #2661,
//! and for a long time that value was only ever **logged** —
//! `dispatch/task_dispatch.rs` emitted one `tracing::info!` line and returned.
//! Nothing wrote it anywhere, so `ControllerNotAdmitting` and
//! `AuthorityUnavailable { detail }` could not be joined against from here at
//! all. That is why this section could watch a five-hour, board-wide outage and
//! report `unexplained`.
//!
//! It is now persisted to `build_admission_denials` (migration 161) by the
//! process that made the decision, and read back here as
//! `build_admission_denial`. Two properties make it safe to trust:
//!
//! * the row is **deleted** on the permitted path, so a denial that stopped
//!   applying leaves nothing behind — without that this table would be the
//!   #2661 tombstone in a new location; and
//! * every read carries `denied_at` and `age_seconds`, and a reason is only
//!   emitted while the record is **fresh** ([`DENIAL_FRESHNESS_SECONDS`]). A
//!   stale row is still reported, marked `fresh: false`, and never blamed.
//!
//! The controller's readiness itself remains **process-local** — an
//! `AtomicBool`/`AtomicU64` set on the leader — so no query from here can read
//! it live. A read-live MCP tool was considered and rejected:
//! `mark_topology_ready` is reachable only from `become_leader`, so a standby
//! serving the call would answer `TopologyPending` every time, which is a
//! confident wrong answer rather than a missing one. `readiness` therefore
//! arrives here the only honest way — carried on the persisted denial written
//! by whichever process actually refused.

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
    "build_admission_create_unknown",
    "build_admission_denial",
];

/// Gates the real dispatcher applies that this section cannot see.
///
/// Not exhaustive and deliberately not silent: every entry here is a way a task
/// can be left queued while this section reports `unexplained`.
///
/// The first group is the per-task gate sequence in `djinn-coordinator`'s
/// `dispatch/task_dispatch.rs`. The second group is the
/// `BuildAdmissionReadiness` ladder: **every** one of those states makes
/// `admit()` return `ControllerNotAdmitting` for every task on the board,
/// before occupancy is measured. That entire group was missing from this list
/// until 2026-07-29, when the board spent five hours wedged behind one of
/// them while this section reported `unexplained` — the hole was exactly where
/// the outage was.
///
/// Only `create_unknown_health` left a durable row and so moved into
/// [`EVALUATED_GATES`] (as `build_admission_create_unknown`). The rest are
/// process-local atomics on the leader and stay honestly unevaluated.
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
    // BuildAdmissionReadiness, fail-closed priority order. See
    // `djinn_coordinator::build_admission::BuildAdmissionReadiness`.
    "build_admission_shutdown_draining",
    "build_admission_journal_recovery_incomplete",
    "build_admission_journal_unhealthy",
    "build_admission_seeded_occupancy_above_cap",
    "build_admission_inventory_pending",
    "build_admission_topology_pending",
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

/// The reclaim settle window, mirrored from
/// `djinn_coordinator::build_admission_inventory::DEFAULT_RECLAIM_SETTLE_WINDOW`.
///
/// `djinn-db` sits below `djinn-coordinator` and cannot import the constant, so
/// it is restated with the reference. It is not a tuning knob here: it is the
/// threshold that separates a `create_unknown` row that is the ordinary
/// intermediate state of a healthy in-flight dispatch (POST accepted, session
/// callback not yet delivered) from one that reconciliation itself would
/// consider settled.
const RECLAIM_SETTLE_WINDOW_SECONDS: i64 = 300;

/// The durable half of the build-admission controller's readiness.
///
/// # What this proves, and what it does not
///
/// A row in `admission_journal.state = 'create_unknown'` means a create was
/// POSTed and its object UID was never learned. It occupies journal capacity
/// for as long as it lasts, and the controller's `create_unknown_pending` gate
/// — which returns `BuildAdmissionReadiness::CreateUnknownHealth` and denies
/// **every** enforced admission before occupancy is measured — is seeded by
/// counting exactly this population.
///
/// It does **not** prove the gate is currently armed. Three reasons, all of
/// them stated in the payload rather than papered over:
///
/// 1. The controller's mode (`buildAdmission.mode`) is process configuration,
///    not a durable row. Under `off`/`observe` the readiness gate denies
///    nothing.
/// 2. Since #2746 only rows belonging to a **predecessor** epoch arm the gate;
///    this section cannot know the live process's `creator_server_epoch`, so
///    `distinct_creator_epochs` is reported and the inference is left to the
///    reader.
/// 3. The gate itself is an in-memory `AtomicU64` on the leader.
///
/// So the reason this signal contributes is scoped to what the rows do prove:
/// a `create_unknown` row untouched for longer than the reclaim settle window
/// is, by construction, no longer a healthy POST→session window.
#[derive(Clone, Debug, Default)]
pub(super) struct AdmissionJournalSignal {
    /// All rows currently in `create_unknown`, healthy in-flight ones included.
    pub create_unknown_active: i64,
    /// Of those, the ones untouched for longer than the reclaim settle window.
    /// This is the population that cannot be explained as normal dispatch.
    pub create_unknown_settled: i64,
    /// Distinct `creator_server_epoch` values across the active population.
    /// More than one means at least one row is definitely a predecessor's.
    pub distinct_creator_epochs: i64,
    /// `updated_at` of the oldest active `create_unknown` row.
    pub oldest_create_unknown_at: Option<String>,
}

/// What this section managed to learn about the `admission_journal` authority.
#[derive(Clone, Debug)]
pub(super) enum JournalSignal {
    Observed(AdmissionJournalSignal),
    Unobservable { detail: &'static str },
}

/// How recent a persisted denial must be to be blamed for a strand.
///
/// The coordinator re-attempts a stranded task on every dispatch tick, so a
/// genuinely blocked task keeps its record fresh; a record older than this
/// belongs to a task the dispatcher has stopped considering, and blaming it
/// would resurrect the replayed-tombstone failure of #2661. Stale records are
/// still REPORTED — with `fresh: false` and an `age_seconds` — because "the
/// last thing that happened was this, a while ago" is real evidence. It is
/// just not a current reason.
const DENIAL_FRESHNESS_SECONDS: i64 = 900;

/// The dispatcher's own recorded denial for one task, from
/// `build_admission_denials` (migration 161).
#[derive(Clone, Debug)]
pub(super) struct DenialRow {
    /// `at_capacity` / `controller_not_admitting` / `authority_unavailable`.
    pub cause: String,
    /// The closed readiness gate, for `controller_not_admitting`. This is the
    /// field that existed only in container logs during the outage.
    pub readiness: Option<String>,
    /// The capacity authority's own words, for `authority_unavailable`.
    pub detail: Option<String>,
    /// Occupancy as MEASURED. `None` means the denial never consulted it,
    /// which is true of every readiness denial.
    pub occupancy: Option<i64>,
    pub cap: i64,
    /// The deciding process's admission epoch.
    pub server_epoch: String,
    /// Start of the uninterrupted denial streak.
    pub first_denied_at: Option<String>,
    pub denied_at: Option<String>,
    pub denial_count: i64,
    /// Seconds since `denied_at`, computed by the database so this section
    /// never has to compare its own clock with Postgres's.
    pub age_seconds: i64,
}

/// What this section managed to learn about the build-lease ledger.
///
/// `Unobservable` is a first-class outcome on purpose. Falling back to "no
/// rows" on a failed read is exactly the mistake this module exists to undo: it
/// would let an unread ledger masquerade as a clean one. The journal authority
/// is carried in the same value but fails independently: an unreadable
/// `admission_journal` must not silence the lease evidence, or vice versa.
#[derive(Clone, Debug)]
pub(super) enum LeaseLedger {
    Observed {
        by_task: HashMap<String, DispatchLeaseRow>,
        capacity: LeaseCapacity,
        journal: JournalSignal,
        /// The dispatcher's own recorded denial, keyed by task id. Absent when
        /// `build_admission_denials` could not be read, which is reported
        /// rather than silently read as "nobody was denied".
        denials: Option<HashMap<String, DenialRow>>,
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
    /// The `admission_journal` readiness evidence as JSON, or `null` when that
    /// authority could not be read.
    pub build_admission: serde_json::Value,
    /// False when `admission_journal` could not be read, which moves
    /// `build_admission_create_unknown` from evaluated to unevaluated.
    pub journal_evaluated: bool,
    /// Why the journal gate could not be evaluated.
    pub journal_unevaluated_detail: Option<&'static str>,
    /// The dispatcher's own recorded denial for this task as JSON, or `null`
    /// when it has none (or the table was unreadable).
    pub build_admission_denial: serde_json::Value,
    /// False when `build_admission_denials` could not be read.
    pub denial_evaluated: bool,
    /// Why the denial gate could not be evaluated.
    pub denial_unevaluated_detail: Option<&'static str>,
}

/// Load the `task_dispatch` lease ledger, the pool capacity, and the
/// `admission_journal` readiness evidence.
///
/// All are runtime `sqlx::query` calls (no offline-cache entries), matching the
/// rest of the board-health sections. Either lease query failing yields
/// [`LeaseLedger::Unobservable`] — never a silently empty ledger. The journal
/// query failing degrades only the journal gate, because the two are separate
/// authorities and an outage in one is not evidence about the other.
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

    LeaseLedger::Observed {
        by_task,
        capacity,
        journal: load_journal_signal(pool).await,
        denials: load_denials(pool).await,
    }
}

/// Load the dispatcher's own recorded denials, keyed by task id.
///
/// `None` on a failed read. Falling back to an empty map would let an unread
/// table assert that nothing was denied, which is the exact substitution this
/// module exists to prevent.
async fn load_denials(pool: &sqlx::PgPool) -> Option<HashMap<String, DenialRow>> {
    let sql = r#"SELECT consumer_id,
                        cause,
                        readiness,
                        detail,
                        occupancy::BIGINT AS occupancy,
                        cap::BIGINT       AS cap,
                        server_epoch,
                        to_char(first_denied_at AT TIME ZONE 'utc',
                                'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS first_denied_at,
                        to_char(denied_at AT TIME ZONE 'utc',
                                'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS denied_at,
                        denial_count::BIGINT AS denial_count,
                        FLOOR(EXTRACT(EPOCH FROM (now() - denied_at)))::BIGINT AS age_seconds
                   FROM build_admission_denials
                  WHERE consumer_kind = 'task_dispatch'"#;

    let rows = sqlx::query(sql).fetch_all(pool).await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|row| {
                let consumer_id: String = row.try_get("consumer_id").ok()?;
                Some((
                    consumer_id,
                    DenialRow {
                        cause: row.try_get("cause").ok()?,
                        readiness: row.try_get("readiness").ok().flatten(),
                        detail: row.try_get("detail").ok().flatten(),
                        occupancy: row.try_get("occupancy").ok().flatten(),
                        cap: row.try_get("cap").unwrap_or(0),
                        server_epoch: row.try_get("server_epoch").unwrap_or_default(),
                        first_denied_at: row.try_get("first_denied_at").ok().flatten(),
                        denied_at: row.try_get("denied_at").ok().flatten(),
                        denial_count: row.try_get("denial_count").unwrap_or(0),
                        age_seconds: row.try_get("age_seconds").unwrap_or(i64::MAX),
                    },
                ))
            })
            .collect(),
    )
}

/// Count the durable `create_unknown` population that seeds the controller's
/// `CreateUnknownHealth` readiness gate.
///
/// One aggregate over `admission_journal` (migration 121). No migration and no
/// new write path: `create_unknown` is already a state the lifecycle writes,
/// and `admission_journal_occupancy_idx` already covers `(domain, state)` for
/// the occupying states.
async fn load_journal_signal(pool: &sqlx::PgPool) -> JournalSignal {
    // `state = 'create_unknown'` is by construction active: `terminal` is a
    // distinct state and the schema's terminal check enforces the split.
    let sql = r#"SELECT
             COUNT(*)::BIGINT AS create_unknown_active,
             COUNT(*) FILTER (
                 WHERE updated_at < now() - make_interval(secs => $1)
             )::BIGINT AS create_unknown_settled,
             COUNT(DISTINCT creator_server_epoch)::BIGINT AS distinct_creator_epochs,
             to_char(MIN(updated_at) AT TIME ZONE 'utc',
                     'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS oldest_create_unknown_at
           FROM admission_journal
          WHERE state = 'create_unknown'"#;

    let Ok(row) = sqlx::query(sql)
        .bind(RECLAIM_SETTLE_WINDOW_SECONDS as f64)
        .fetch_one(pool)
        .await
    else {
        return JournalSignal::Unobservable {
            detail: "admission_journal could not be read",
        };
    };

    JournalSignal::Observed(AdmissionJournalSignal {
        create_unknown_active: row.try_get("create_unknown_active").unwrap_or(0),
        create_unknown_settled: row.try_get("create_unknown_settled").unwrap_or(0),
        distinct_creator_epochs: row.try_get("distinct_creator_epochs").unwrap_or(0),
        oldest_create_unknown_at: row.try_get("oldest_create_unknown_at").ok().flatten(),
    })
}

/// Apply the `admission_journal` readiness gate.
///
/// The reason fires on `create_unknown_settled`, not on `create_unknown_active`.
/// That distinction is the whole design: `finish_task_run_build_admission`
/// writes `create_unknown` for **every** task-run the instant the slot pool
/// accepts the create, and it stays that way until the `("session","started")`
/// callback supplies the object UID. Firing on the active count would emit a
/// denial reason during every healthy dispatch — the precise class of
/// fabricated reason this module exists to remove. A row untouched for longer
/// than the reclaim settle window cannot be that healthy window.
fn journal_gate(
    journal: &JournalSignal,
) -> (
    serde_json::Value,
    Vec<&'static str>,
    bool,
    Option<&'static str>,
) {
    let signal = match journal {
        JournalSignal::Unobservable { detail } => {
            return (serde_json::Value::Null, Vec::new(), false, Some(detail));
        }
        JournalSignal::Observed(signal) => signal,
    };

    let settled = signal.create_unknown_settled > 0;
    let payload = serde_json::json!({
        "authority": "admission_journal",
        "create_unknown_active":    signal.create_unknown_active,
        "create_unknown_settled":   signal.create_unknown_settled,
        "settle_window_seconds":    RECLAIM_SETTLE_WINDOW_SECONDS,
        "distinct_creator_epochs":  signal.distinct_creator_epochs,
        "oldest_create_unknown_at": signal.oldest_create_unknown_at,
        "note": "The build-admission controller seeds its `CreateUnknownHealth` \
                 readiness gate from this population, and that gate denies EVERY \
                 enforced admission with `controller_not_admitting` BEFORE occupancy \
                 is measured — so `build_capacity` can look entirely healthy while \
                 nothing dispatches. A settled row (older than \
                 `settle_window_seconds`) is no longer explainable as a healthy \
                 POST→session window. Whether the gate is ARMED additionally \
                 depends on the controller's mode and on the row belonging to a \
                 predecessor epoch, neither of which is durable; see \
                 `/debug/dispatch-state` for the live readiness.",
    });

    let reasons = if settled {
        vec!["admission_create_unknown_pending"]
    } else {
        Vec::new()
    };
    (payload, reasons, true, None)
}

/// Apply the persisted-denial gate to one task.
///
/// This is the only gate in this module that reports the dispatcher's OWN
/// decision rather than a re-derivation of it, so it is the only one that can
/// never disagree with what actually happened.
fn denial_gate(
    denials: Option<&HashMap<String, DenialRow>>,
    task_id: &str,
) -> (
    serde_json::Value,
    Vec<&'static str>,
    bool,
    Option<&'static str>,
) {
    let Some(denials) = denials else {
        return (
            serde_json::Value::Null,
            Vec::new(),
            false,
            Some("build_admission_denials could not be read"),
        );
    };
    let Some(row) = denials.get(task_id) else {
        // No row is a real answer: the permitted path DELETES it. This gate
        // was evaluated and found nothing.
        return (serde_json::Value::Null, Vec::new(), true, None);
    };

    let fresh = row.age_seconds <= DENIAL_FRESHNESS_SECONDS;
    let payload = serde_json::json!({
        "cause":            row.cause,
        "readiness":        row.readiness,
        "detail":           row.detail,
        // `null`, never 0. A readiness denial measures no occupancy at all,
        // and the fabricated zero is what made a tombstoned lease read as a
        // full pool for forty minutes (#2661).
        "occupancy":        row.occupancy,
        "cap":              row.cap,
        "server_epoch":     row.server_epoch,
        "first_denied_at":  row.first_denied_at,
        "denied_at":        row.denied_at,
        "denial_count":     row.denial_count,
        "age_seconds":      row.age_seconds,
        "fresh":            fresh,
        "freshness_window_seconds": DENIAL_FRESHNESS_SECONDS,
        "note": "The dispatcher's OWN `DenialCause`, written by the process that \
                 refused (migration 161). The row is deleted when the task is \
                 admitted, so its presence means the most recent decision was a \
                 denial. A record older than `freshness_window_seconds` is reported \
                 but not blamed: the dispatcher retries a stranded task every tick, \
                 so a stale row belongs to a task it has stopped considering.",
    });

    let reasons = if fresh {
        vec![match row.cause.as_str() {
            "at_capacity" => "build_admission_denied_at_capacity",
            "controller_not_admitting" => "build_admission_denied_controller_not_admitting",
            "authority_unavailable" => "build_admission_denied_authority_unavailable",
            // The CHECK constraint bounds the column to the three above, so
            // this is unreachable through the write path. It exists so a
            // future cause cannot be silently dropped into an empty `reasons`
            // — the failure this whole module was written to remove.
            _ => "build_admission_denied_unrecognized_cause",
        }]
    } else {
        Vec::new()
    };
    (payload, reasons, true, None)
}

/// Apply the build-lease gate to one task.
///
/// Every reason produced here is a statement about a durable row, never an
/// inference about what the dispatcher "probably" did.
pub(super) fn lease_gate(ledger: &LeaseLedger, task_id: &str) -> LeaseGateOutcome {
    let (by_task, capacity, journal, denials) = match ledger {
        LeaseLedger::Unobservable { detail } => {
            return LeaseGateOutcome {
                build_lease: serde_json::Value::Null,
                build_capacity: serde_json::Value::Null,
                reasons: Vec::new(),
                evaluated: false,
                unevaluated_detail: Some(detail),
                build_admission: serde_json::Value::Null,
                journal_evaluated: false,
                journal_unevaluated_detail: Some(
                    "build_leases could not be read; the journal authority was not consulted",
                ),
                build_admission_denial: serde_json::Value::Null,
                denial_evaluated: false,
                denial_unevaluated_detail: Some(
                    "build_leases could not be read; recorded denials were not consulted",
                ),
            };
        }
        LeaseLedger::Observed {
            by_task,
            capacity,
            journal,
            denials,
        } => (by_task, capacity, journal, denials.as_ref()),
    };

    // The journal authority is board-wide, not per-task: `CreateUnknownHealth`
    // denies EVERY admission, which is exactly what made it invisible per-task.
    let (build_admission, journal_reasons, journal_evaluated, journal_unevaluated_detail) =
        journal_gate(journal);
    // The dispatcher's own recorded decision for THIS task.
    let (build_admission_denial, denial_reasons, denial_evaluated, denial_unevaluated_detail) =
        denial_gate(denials, task_id);

    let at_capacity = capacity.cap > 0 && capacity.occupancy >= capacity.cap;
    // `enforcing` used to be the bare field name. During the 2026-07-29 outage
    // this block read `{occupancy: 1, cap: 3, enforcing: true, at_capacity:
    // false}` — an operator reasonably concluded capacity was fine and looked
    // elsewhere, while the JOURNAL authority was denying every dispatch before
    // capacity was ever measured. Both nouns in the old payload pointed at the
    // lease authority without ever saying so. Every key here now names which
    // authority it speaks for.
    let build_capacity = serde_json::json!({
        "authority": "build_leases",
        "occupancy": capacity.occupancy,
        "cap":       capacity.cap,
        "lease_authority_enforcing": capacity.enforcing,
        "at_capacity": at_capacity,
        "note": "LEASE authority only (`build_leases` / `build_lease_caps`, armed by \
                 `admission_handoff.v1_mode`). `lease_authority_enforcing` says whether \
                 THIS authority is armed and says NOTHING about the build-admission \
                 controller, whose mode is process configuration. A dispatch must clear \
                 both; see `build_admission` for the other one. These numbers are only \
                 reached AFTER the controller's readiness gate passes, so \
                 `at_capacity: false` here is not evidence that a dispatch can proceed.",
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
            // The journal authority and the recorded denial are independent of
            // the lease authority's arming: they are what denied every dispatch
            // on 2026-07-29 while the lease side looked idle. Dropping their
            // reasons here would restore exactly the blind spot.
            reasons: journal_reasons.into_iter().chain(denial_reasons).collect(),
            evaluated: true,
            unevaluated_detail: None,
            build_admission,
            journal_evaluated,
            journal_unevaluated_detail,
            build_admission_denial,
            denial_evaluated,
            denial_unevaluated_detail,
        };
    }

    let mut reasons: Vec<&'static str> = journal_reasons;
    reasons.extend(denial_reasons);
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
        build_admission,
        journal_evaluated,
        journal_unevaluated_detail,
        build_admission_denial,
        denial_evaluated,
        denial_unevaluated_detail,
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
        .filter(|gate| lease.journal_evaluated || *gate != "build_admission_create_unknown")
        .filter(|gate| lease.denial_evaluated || *gate != "build_admission_denial")
        .collect();
    evaluated_gates.sort_unstable();

    let mut unevaluated_gates: Vec<&'static str> = UNEVALUATED_GATES.to_vec();
    if !lease.evaluated {
        unevaluated_gates.push("build_lease_admission");
    }
    if !lease.journal_evaluated {
        unevaluated_gates.push("build_admission_create_unknown");
    }
    if !lease.denial_evaluated {
        unevaluated_gates.push("build_admission_denial");
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
        "build_admission":      lease.build_admission,
        "build_admission_denial": lease.build_admission_denial,
        "gate_verdict":         gate_verdict,
        "reasons":              reasons,
        "coverage": serde_json::json!({
            "scope":             "partial",
            "evaluated_gates":   evaluated_gates,
            "unevaluated_gates": unevaluated_gates,
            "build_lease_unevaluated_detail": lease.unevaluated_detail,
            "build_admission_unevaluated_detail": lease.journal_unevaluated_detail,
            "build_admission_denial_unevaluated_detail": lease.denial_unevaluated_detail,
            "note": "`reasons` covers only `evaluated_gates`. An empty `reasons` \
                     yields `unexplained`, which means no evaluated gate fired — \
                     NOT that the dispatcher had no reason. Two authorities gate a \
                     dispatch: the LEASE ledger (`build_capacity`) and the \
                     build-admission JOURNAL (`build_admission`). The journal one \
                     denies before occupancy is ever measured, so a healthy \
                     `build_capacity` is not evidence that dispatch is possible. \
                     `build_admission_denial` is the dispatcher's OWN recorded \
                     `DenialCause` (#2661, migration 161) — the only field here \
                     that is a decision rather than a re-derivation of one.",
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
}
