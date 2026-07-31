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
//! # The lease authority, and what it can prove
//!
//! The **lease authority** (`build_leases` + `build_lease_caps`, armed by
//! the invocation-lease authority) is the weighted FIFO, and the only durable
//! surface that ever measures pool occupancy. It is read here, and every key
//! in the emitted `build_capacity` block names it explicitly.
//!
//! What it cannot prove is that a dispatch is *possible*. Occupancy is only
//! ever consulted **after** the build-admission controller has agreed to admit
//! at all, and that decision is process-local. On 2026-07-29 the lease side
//! reported a perfectly healthy pool (`occupancy: 1, cap: 3, at_capacity:
//! false`) for five hours while `admit()` refused every task before capacity
//! was measured, and this section reported `gate_verdict: "unexplained"` with
//! `reasons: []` once every thirty seconds about a completely wedged board.
//! The only surface that named the cause was a `readiness=` field in a
//! container log. That is the hole the next section closes.
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
//!
//! # What the Kueue cutover changed under this section (o53p / ubne / plcj)
//!
//! Two of the sources above lost their producer, and saying so is the whole
//! point of this module.
//!
//! * **`build_admission_denials` has no writer.** Its only producer was
//!   `dispatch/task_dispatch.rs`'s `record_task_run_denial`, rendering a
//!   `DenialCause` from the pre-create `BuildAdmissionController`; both were
//!   deleted. The relation, the repository and the read below are RETAINED
//!   deliberately — a `null` denial is the literal truth ("no recorded denial")
//!   and legacy rows are still surfaced — but the steady state is now `null`,
//!   and a new writer must NOT be added: a denial recorded against a deleted
//!   authority is the replayed tombstone of #2661 in a new table.
//! * **`task_dispatch` build-lease rows are legacy-only.** Nothing constructs
//!   `LeaseIdentity::TaskDispatch` any more — `djinn-coordinator`'s
//!   `no_dispatch_lease_can_ever_be_acquired_again` fails if a constructor
//!   returns — so `build_lease_queued` / `build_lease_terminal` /
//!   `build_lease_occupied_without_session` can only fire for rows that predate
//!   the cutover.
//!
//! What is NOT inert is the part that answers "can this board dispatch at all":
//! `build_capacity` is a ledger-wide `SUM(weight)` across EVERY consumer kind,
//! and `task_invocation` leases are still written, from inside the task-run
//! Pod, by the per-invocation cgroup lease that 9oga's non-goals retain. So
//! `build_pool_at_capacity` remains a live, durable blocking reason, and the
//! integration test `a_pool_filled_by_live_invocation_leases_blocks_the_gate`
//! asserts it against a real ledger rather than against a legacy row shape.
//!
//! The blind spot the cutover OPENED is that Kueue's ClusterQueue now decides
//! build capacity, and a Job it has not admitted is queued by Kueue with no row
//! in any relation this section reads. That is why
//! `kueue_clusterqueue_admission` was listed in [`UNEVALUATED_GATES`] rather
//! than left for an operator to infer from a healthy-looking `build_capacity`.
//!
//! # Closing that blind spot (ecrz)
//!
//! It is no longer true that Kueue's decision leaves no row. Migration 165's
//! `kueue_workload_admission` is a durable projection of what Kueue decided,
//! written by the leader's Workload reflector, and
//! [`super::board_health_kueue_admission`] now reads it into a `kueue_admission`
//! block beside `build_capacity`. `kueue_clusterqueue_admission` therefore moves
//! between `evaluated_gates` and `unevaluated_gates` at runtime instead of
//! sitting unconditionally in the latter — but ONLY when the projection holds
//! rows. Read that module for why an empty relation is deliberately not treated
//! as an evaluation: it is what a healthy unarmed cluster and a dead reflector
//! both look like from Postgres, and this section does not emit verdicts it
//! cannot justify.

use std::collections::HashMap;

use sqlx::Row;

use super::board_health_kueue_admission::{KUEUE_GATE, KueueGateOutcome};

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
    "build_admission_denial",
];

/// Gates the real dispatcher applies that this section cannot see.
///
/// Not exhaustive and deliberately not silent: every entry here is a way a task
/// can be left queued while this section reports `unexplained`.
///
/// These are the per-task gate sequence in `djinn-coordinator`'s
/// `dispatch/task_dispatch.rs`. The build-admission controller's own readiness
/// ladder is not listed here because it is process-local state on the leader
/// and leaves no durable row to enumerate against; when it refuses, the reason
/// arrives on the persisted denial record instead (`build_admission_denial`).
///
/// `kueue_clusterqueue_admission` is deliberately NOT in this list any more: it
/// is added to whichever list the projection read justifies, per call. See
/// [`super::board_health_kueue_admission`].
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
///
/// **Legacy population.** The pre-create dispatch reservation was stood down by
/// the Kueue cutover and nothing constructs `LeaseIdentity::TaskDispatch` any
/// more, so this population can only shrink. The rows are still read because a
/// board wedged behind one from before the cutover must still be explained.
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
    /// True only when the durable invocation-lease authority is armed to
    /// [`crate::InvocationLeaseMode::Enforce`]. While it is `off`/`shadow` — or
    /// the authority row is absent entirely — the lease FIFO writes no dispatch
    /// rows and cannot be denying anything. A missing or unparseable mode is a
    /// real `false`, not an unobservable capacity read.
    pub enforcing: bool,
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
/// would let an unread ledger masquerade as a clean one. The recorded denials
/// are carried in the same value but fail independently: an unreadable
/// `build_admission_denials` must not silence the lease evidence, or vice
/// versa.
#[derive(Clone, Debug)]
pub(super) enum LeaseLedger {
    Observed {
        by_task: HashMap<String, DispatchLeaseRow>,
        capacity: LeaseCapacity,
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
    /// The dispatcher's own recorded denial for this task as JSON, or `null`
    /// when it has none (or the table was unreadable).
    pub build_admission_denial: serde_json::Value,
    /// False when `build_admission_denials` could not be read.
    pub denial_evaluated: bool,
    /// Why the denial gate could not be evaluated.
    pub denial_unevaluated_detail: Option<&'static str>,
}

/// Load the `task_dispatch` lease ledger, the pool capacity, and the
/// dispatcher's own recorded denials.
///
/// All are runtime `sqlx::query` calls (no offline-cache entries), matching the
/// rest of the board-health sections. Either lease query failing yields
/// [`LeaseLedger::Unobservable`] — never a silently empty ledger. The denial
/// query failing degrades only the denial gate, because it records a decision
/// rather than a re-derivation and an outage in one is not evidence about the
/// other.
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
    // same cap. The invocation-lease authority says whether that cap is armed.
    //
    // The mode is selected as the durable string and parsed through
    // `InvocationLeaseMode`, not compared to a literal here: a value nobody can
    // name must not read as "armed" or as "disarmed" by accident, and there is
    // exactly one place that decides what "enforcing" means.
    let capacity_sql = r#"SELECT
             COALESCE((SELECT SUM(weight) FROM build_leases
                        WHERE state IN ('granted','launching','bound','active','suspect')),
                      0)::BIGINT AS occupancy,
             COALESCE((SELECT cap FROM build_lease_caps WHERE singleton), 0)::BIGINT AS cap,
             (SELECT v1_mode FROM admission_handoff WHERE name = 'build') AS mode"#;

    let Ok(capacity_row) = sqlx::query(capacity_sql).fetch_one(pool).await else {
        return LeaseLedger::Unobservable {
            detail: "build-lease capacity could not be read",
        };
    };

    // An absent authority row is a real, reportable verdict — a deployment that
    // has never armed the authority is disarmed — not an unobservable one.
    let mode = capacity_row
        .try_get::<Option<String>, _>("mode")
        .ok()
        .flatten()
        .and_then(|mode| crate::InvocationLeaseMode::parse(&mode).ok());
    let capacity = LeaseCapacity {
        occupancy: capacity_row.try_get("occupancy").unwrap_or(0),
        cap: capacity_row.try_get("cap").unwrap_or(0),
        enforcing: mode.is_some_and(crate::InvocationLeaseMode::is_enforcing),
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
        //
        // Post-cutover this is also the steady state, because the writer is
        // gone (see the module docs). It stays EVALUATED rather than becoming
        // unevaluated because the claim it makes — "this relation records no
        // denial for this task" — is still exactly true, and the thing it must
        // not be mistaken for is disclosed as `kueue_clusterqueue_admission` in
        // `unevaluated_gates` instead of being smuggled in here.
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
                 so a stale row belongs to a task it has stopped considering. \
                 The Kueue cutover deleted the pre-create controller that WROTE \
                 these rows, so any record you see here predates it; a `null` is \
                 now the steady state and is NOT evidence that Kueue admitted \
                 the Job — see `kueue_clusterqueue_admission` in \
                 `coverage.unevaluated_gates`.",
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
    let (by_task, capacity, denials) = match ledger {
        LeaseLedger::Unobservable { detail } => {
            return LeaseGateOutcome {
                build_lease: serde_json::Value::Null,
                build_capacity: serde_json::Value::Null,
                reasons: Vec::new(),
                evaluated: false,
                unevaluated_detail: Some(detail),
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
            denials,
        } => (by_task, capacity, denials.as_ref()),
    };

    // The dispatcher's own recorded decision for THIS task.
    let (build_admission_denial, denial_reasons, denial_evaluated, denial_unevaluated_detail) =
        denial_gate(denials, task_id);

    let at_capacity = capacity.cap > 0 && capacity.occupancy >= capacity.cap;
    // `enforcing` used to be the bare field name. During the 2026-07-29 outage
    // this block read `{occupancy: 1, cap: 3, enforcing: true, at_capacity:
    // false}` — an operator reasonably concluded capacity was fine and looked
    // elsewhere, while the build-admission controller was refusing every
    // dispatch before capacity was ever measured. Both nouns in the old payload
    // pointed at the lease authority without ever saying so. Every key here now
    // names which authority it speaks for.
    let build_capacity = serde_json::json!({
        "authority": "build_leases",
        "occupancy": capacity.occupancy,
        "cap":       capacity.cap,
        "lease_authority_enforcing": capacity.enforcing,
        "at_capacity": at_capacity,
        "note": "LEASE authority only (`build_leases` / `build_lease_caps`, armed by the \
                 durable invocation-lease authority). `lease_authority_enforcing` says whether \
                 THIS authority is armed and says NOTHING about any other admission \
                 authority. `occupancy` is a ledger-wide SUM(weight) across every \
                 consumer kind, so it is live: `task_invocation` leases are written \
                 from inside the task-run Pod by the per-invocation cgroup lease. \
                 It is still NOT sufficient — since the Kueue cutover a Job can sit \
                 suspended behind a ClusterQueue quota with nothing recorded here — \
                 so `at_capacity: false` is not evidence that a dispatch can proceed; \
                 see `build_admission_denial` for what a dispatcher actually recorded, \
                 and `coverage.unevaluated_gates` for what nothing records at all.",
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
            // The recorded denial is independent of the lease authority's
            // arming: it is what denied every dispatch on 2026-07-29 while the
            // lease side looked idle. Dropping its reasons here would restore
            // exactly the blind spot.
            reasons: denial_reasons,
            evaluated: true,
            unevaluated_detail: None,
            build_admission_denial,
            denial_evaluated,
            denial_unevaluated_detail,
        };
    }

    let mut reasons: Vec<&'static str> = denial_reasons;
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
    kueue: KueueGateOutcome,
    mut reasons: Vec<&'static str>,
) -> serde_json::Value {
    reasons.extend(lease.reasons);
    reasons.extend(kueue.reasons);

    let mut evaluated_gates: Vec<&'static str> = EVALUATED_GATES
        .iter()
        .copied()
        .filter(|gate| lease.evaluated || *gate != "build_lease_admission")
        .filter(|gate| lease.denial_evaluated || *gate != "build_admission_denial")
        .collect();
    // The one gate whose evaluated/unevaluated status is decided per call: it
    // depends on whether the projection had anything to say at all.
    if kueue.evaluated {
        evaluated_gates.push(KUEUE_GATE);
    }
    evaluated_gates.sort_unstable();

    let mut unevaluated_gates: Vec<&'static str> = UNEVALUATED_GATES.to_vec();
    if !lease.evaluated {
        unevaluated_gates.push("build_lease_admission");
    }
    if !lease.denial_evaluated {
        unevaluated_gates.push("build_admission_denial");
    }
    if !kueue.evaluated {
        unevaluated_gates.push(KUEUE_GATE);
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
        "build_admission_denial": lease.build_admission_denial,
        "kueue_admission":      kueue.kueue_admission,
        "kueue_workload":       kueue.kueue_workload,
        "gate_verdict":         gate_verdict,
        "reasons":              reasons,
        "coverage": serde_json::json!({
            "scope":             "partial",
            "evaluated_gates":   evaluated_gates,
            "unevaluated_gates": unevaluated_gates,
            "build_lease_unevaluated_detail": lease.unevaluated_detail,
            "build_admission_denial_unevaluated_detail": lease.denial_unevaluated_detail,
            "kueue_admission_unevaluated_detail": kueue.unevaluated_detail,
            "note": "`reasons` covers only `evaluated_gates`. An empty `reasons` \
                     yields `unexplained`, which means no evaluated gate fired — \
                     NOT that the dispatcher had no reason. `build_capacity` speaks \
                     for the LEASE ledger alone; what Kueue decided is a separate \
                     authority and is reported in `kueue_admission`, read from the \
                     migration-165 projection. \
                     `build_admission_denial` is the dispatcher's OWN recorded \
                     `DenialCause` (#2661, migration 161) — the only field here \
                     that is a decision rather than a re-derivation of one — but \
                     the process that wrote it was deleted by the cutover, so a \
                     `null` there is the steady state rather than an all-clear. \
                     `kueue_clusterqueue_admission` appears in `evaluated_gates` \
                     only while the projection holds rows; while it is empty an \
                     unarmed cluster and a stopped reflector are indistinguishable \
                     from here, so the gate stays unevaluated.",
        }),
        // Retained for backward compatibility with the initial board_health
        // contract.
        "last_dispatched_role": last_dispatched_role,
        "cooldown_until":       cooldown_until,
    })
}

#[cfg(test)]
#[path = "board_health_dispatch_gate_tests.rs"]
mod tests;
