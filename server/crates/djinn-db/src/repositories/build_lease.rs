//! Durable v1 build-lease ledger.
//!
//! Every mutation takes the same transaction-scoped advisory lock.  This makes
//! FIFO selection and occupancy accounting a single serializable decision while
//! retaining terminal rows for idempotent replay and audit.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::{Database, Error as DbError, Result as DbResult};

const OCCUPYING: [&str; 5] = ["granted", "launching", "bound", "active", "suspect"];

/// Every state that is not `terminal`.
///
/// Wider than [`OCCUPYING`] by exactly one state, `queued`, and the difference
/// is load-bearing. A `queued` row holds no capacity, so nothing that reasons
/// about occupancy has any cause to look at it — which is how a queued row
/// became the one population with NO reaper at all. `deploy/kueue/preflight.sh`
/// fences the authority cutover on `state <> 'terminal'`, so such a row blocks
/// the cutover indefinitely while every occupancy-shaped sweep reports clean.
const NONTERMINAL: [&str; 6] = [
    "queued",
    "granted",
    "launching",
    "bound",
    "active",
    "suspect",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildLeaseConsumerKind {
    /// Layer 2: the fine, role-AGNOSTIC escalation unit. One invocation whose
    /// measured `cpu.stat` usage crossed the build threshold.
    TaskInvocation,
    /// A full workspace compile dispatched as a Kubernetes Job.
    GraphWarm,
    /// Layer 1: the coarse, role-DERIVED dispatch admission unit, reserved
    /// before a task-run pod was created for work certain to compile.
    ///
    /// # LEGACY ROWS ONLY (Kueue cutover, o53p)
    ///
    /// Nothing acquires one of these any more. The pre-create dispatch
    /// reservation this served was stood down by the Kueue cutover, and
    /// `LeaseIdentity::TaskDispatch` has no constructor left anywhere in the
    /// workspace — Kueue's ClusterQueue admits a suspended Job against a quota
    /// instead, so no capacity is reserved before the object exists.
    ///
    /// The variant is retained because rows written before the cutover are
    /// still in `build_leases` and must still be readable and reclaimable.
    /// `djinn_coordinator::build_lease_reclaim` retires them on exactly that
    /// basis; see its module docs before reintroducing an acquirer.
    TaskDispatch,
}
impl BuildLeaseConsumerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TaskInvocation => "task_invocation",
            Self::GraphWarm => "graph_warm",
            Self::TaskDispatch => "task_dispatch",
        }
    }
    fn parse(s: &str) -> DbResult<Self> {
        match s {
            "task_invocation" => Ok(Self::TaskInvocation),
            "graph_warm" => Ok(Self::GraphWarm),
            "task_dispatch" => Ok(Self::TaskDispatch),
            _ => Err(DbError::InvalidData(format!(
                "invalid build lease consumer kind `{s}`"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildLeaseState {
    Queued,
    Granted,
    Launching,
    Bound,
    Active,
    Suspect,
    Terminal,
}
impl BuildLeaseState {
    fn parse(s: &str) -> DbResult<Self> {
        match s {
            "queued" => Ok(Self::Queued),
            "granted" => Ok(Self::Granted),
            "launching" => Ok(Self::Launching),
            "bound" => Ok(Self::Bound),
            "active" => Ok(Self::Active),
            "suspect" => Ok(Self::Suspect),
            "terminal" => Ok(Self::Terminal),
            _ => Err(DbError::InvalidData(format!(
                "invalid build lease state `{s}`"
            ))),
        }
    }
}

/// The closed set of reasons a build lease can reach `terminal`.
///
/// This exists so the set is a Rust type rather than a scattering of SQL string
/// literals. Every producer below writes [`Self::as_str`] and every consumer
/// matches the enum EXHAUSTIVELY, so adding a reason is a compile error at the
/// mapping sites instead of a silent fall-through.
///
/// That fall-through is not hypothetical: `reclaimed_absent` was added by the
/// dispatch reclaimer with no mapping, landed in a `_ =>` arm that answered
/// `LeaseUnavailable`, and permanently wedged every task whose dispatch lease
/// had been reclaimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildLeaseTerminalReason {
    Abandoned,
    Cancelled,
    Released,
    DeadlineExpired,
    ReclaimedAbsent,
    /// The reclaimer retired this lease because the Kubernetes object it
    /// committed to still EXISTS but has reached a terminal condition
    /// (`Complete`/`Failed`). Distinct from [`Self::ReclaimedAbsent`] so an
    /// operator reading the ledger can tell "its Job was garbage-collected"
    /// apart from "its Job finished and its holder never released".
    ReclaimedFinished,
    /// An operator retired this lease explicitly, via
    /// `djinn-server build-lease clear`. Distinct from every automatic reason
    /// so a human intervention is never mistaken for the reclaimer working.
    OperatorCleared,
}
impl BuildLeaseTerminalReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abandoned => "abandoned",
            Self::Cancelled => "cancelled",
            Self::Released => "released",
            Self::DeadlineExpired => "deadline_expired",
            Self::ReclaimedAbsent => "reclaimed_absent",
            Self::ReclaimedFinished => "reclaimed_finished",
            Self::OperatorCleared => "operator_cleared",
        }
    }
    /// Parse a stored reason. `None` covers both a NULL column and a value
    /// written by a future version of this schema.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "abandoned" => Some(Self::Abandoned),
            "cancelled" => Some(Self::Cancelled),
            "released" => Some(Self::Released),
            "deadline_expired" => Some(Self::DeadlineExpired),
            "reclaimed_absent" => Some(Self::ReclaimedAbsent),
            "reclaimed_finished" => Some(Self::ReclaimedFinished),
            "operator_cleared" => Some(Self::OperatorCleared),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseKey {
    pub consumer_kind: BuildLeaseConsumerKind,
    pub consumer_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueBuildLeaseInput {
    pub key: BuildLeaseKey,
    /// Canonical, stable representation of all immutable invocation/warm fields.
    pub immutable_identity: String,
    /// RFC3339 timestamp, interpreted by PostgreSQL.
    pub queue_deadline: Option<String>,
    /// Absolute deadline retained while queued and carried into its grant.
    pub launch_deadline: Option<String>,
    /// Capacity units this row occupies once granted, in build slots.
    ///
    /// Derived from the rendered manifests by the policy owner
    /// (`djinn_runtime::build_slot_weight`), never hand-set here. Zero is
    /// legal and load-bearing: it is how the non-double-charge rule is
    /// expressed for a `task_invocation` whose task-run already holds an
    /// occupying `task_dispatch` row. A zero-weight row still queues, still
    /// receives a fencing token, and still fences its pod -- it simply buys no
    /// capacity, because capacity was already bought on its behalf at dispatch.
    ///
    /// Immutable once written (enforced by the table trigger): capacity granted
    /// at one weight may never be re-priced underneath its occupant.
    pub weight: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseRow {
    pub key: BuildLeaseKey,
    pub immutable_identity: String,
    pub enqueue_sequence: i64,
    pub fencing_token: Option<i64>,
    pub state: BuildLeaseState,
    /// RFC3339 UTC with millisecond precision, rendered by the shared column
    /// list. `None` means the row carries no deadline at all.
    pub queue_deadline: Option<String>,
    /// RFC3339 UTC with millisecond precision, rendered by the shared column
    /// list. `None` means the row carries no deadline at all.
    pub launch_deadline: Option<String>,
    pub bound_pod_uid: Option<String>,
    pub candidate_cleanup: Option<serde_json::Value>,
    pub terminal_reason: Option<String>,
    /// Capacity units this row occupies while in an occupying state.
    pub weight: i64,
    pub timeout_credit_consumed: bool,
    pub created_at: String,
    pub updated_at: String,
    pub granted_at: Option<String>,
    pub terminal_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueBuildLeaseResult {
    Queued {
        row: BuildLeaseRow,
        idempotent: bool,
        /// `Some(terminal_reason)` when this call retired a spent
        /// `task_dispatch` attempt to mint `row`. Purely diagnostic, but it is
        /// the only place the tombstone that used to wedge dispatch is
        /// observable, so it is carried out to the operator log.
        superseded: Option<String>,
    },
    LeaseIdentityConflict {
        existing: BuildLeaseRow,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum GrantNextBuildLeaseResult {
    Granted(BuildLeaseRow),
    Empty { occupancy: i64, cap: i64 },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseSnapshot {
    pub cap: i64,
    pub occupied: i64,
    pub rows: Vec<BuildLeaseRow>,
}

/// The full observed identity of one occupying lease, gathered *before* its
/// Kubernetes object was proven absent.
///
/// Every field participates in the compare-and-set performed by
/// [`BuildLeaseRepository::reclaim_absent_object`]. `observed_updated_at` is
/// what closes the race against the lease's own holder: any acknowledgement,
/// bind, or status report between the absence proof and the write bumps
/// `updated_at`, and the reclamation is fenced instead of retiring live work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimAbsentBuildLeaseInput {
    pub key: BuildLeaseKey,
    pub observed_state: BuildLeaseState,
    pub observed_immutable_identity: String,
    pub observed_fencing_token: Option<i64>,
    pub observed_bound_pod_uid: Option<String>,
    pub observed_updated_at: String,
    /// Why this lease is being retired, recorded on the row. The caller owns
    /// the proof, so the caller names it: an operator triaging a stale ledger
    /// reads this column, and "the Job was collected" and "the Job finished
    /// and nobody released" call for different follow-up.
    pub terminal_reason: BuildLeaseTerminalReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReclaimAbsentBuildLeaseOutcome {
    Reclaimed(BuildLeaseRow),
    AlreadyTerminal(BuildLeaseRow),
    Fenced { reason: String },
}

/// Atomic repository for queueing and fencing build units.
pub struct BuildLeaseRepository {
    db: Database,
}
impl BuildLeaseRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Queue a stable identity. Exact replay returns the original row; any
    /// immutable mismatch is a typed result rather than a capacity side effect.
    ///
    /// # Terminal rows are replayed -- except for `task_dispatch`
    ///
    /// Retaining a terminal row and replaying it is the whole point of this
    /// ledger for `graph_warm` and `task_invocation`: a lost response must be
    /// answered with the outcome that actually happened (`Released`,
    /// `Cancelled`, a timeout credit) and must never silently re-run finished
    /// work. That behaviour is untouched here.
    ///
    /// `task_dispatch` is the one population where it is wrong, and it wedged
    /// production. Its key is `{task_id}:{generation}` — a *coordinate*, not an
    /// attempt: the dispatcher asks for that same coordinate on every tick for
    /// as long as the task is dispatchable. A terminal row under it is a SPENT
    /// ATTEMPT, occupying nothing, yet replaying it answered `LeaseWaitTimeout`
    /// / `LeaseUnavailable` forever, which layer-1 admission converted into a
    /// denial *before* the journal write that would have advanced the
    /// generation. The key therefore never changed and the denial re-derived
    /// itself every tick: a closed, self-sustaining wedge that survives every
    /// restart. Six dispatchable tasks sat behind it at occupancy 0-1 of cap 3.
    ///
    /// So a terminal `task_dispatch` row is retired and a fresh attempt minted
    /// under the same key. Deleting rather than resetting in place is
    /// deliberate:
    ///
    /// * the row's identity, weight and `bound_pod_uid` are trigger-immutable,
    ///   so a terminal→queued UPDATE would break against the schema the moment
    ///   the configured dispatch weight changed or the row had ever been bound;
    /// * a brand new `enqueue_sequence` sends the new attempt to the BACK of
    ///   the FIFO, where a new arrival belongs — an in-place reset would keep
    ///   the retired attempt's queue position and let it cut the line;
    /// * a brand new `fencing_token` (nextval, never reissued) means any actor
    ///   still holding the retired attempt's token is fenced out, exactly as it
    ///   would be against any other new row.
    ///
    /// No capacity can be double-granted by this: terminal rows are excluded
    /// from [`OCCUPYING`], so the retired row was buying nothing that the fresh
    /// attempt could buy twice. `build_leases` holds dispatch capacity, and
    /// spent capacity is not evidence.
    ///
    /// The separate dispatch lifecycle/audit trail this used to point at
    /// (`admission_journal`) was the pods-quota reservation ledger and was
    /// deleted by the Kueue cutover (o53p).
    pub async fn queue(&self, input: &QueueBuildLeaseInput) -> DbResult<QueueBuildLeaseResult> {
        if input.key.consumer_id.trim().is_empty() || input.immutable_identity.trim().is_empty() {
            return Err(DbError::InvalidData(
                "build lease identity must not be blank".into(),
            ));
        }
        if input.weight < 0 {
            return Err(DbError::InvalidData(
                "build lease weight must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let mut superseded: Option<String> = None;
        if let Some(row) = fetch(&mut tx, &input.key, true).await? {
            let spent_dispatch_attempt = row.state == BuildLeaseState::Terminal
                && input.key.consumer_kind == BuildLeaseConsumerKind::TaskDispatch;
            if !spent_dispatch_attempt {
                tx.commit().await?;
                return Ok(if row.immutable_identity == input.immutable_identity {
                    QueueBuildLeaseResult::Queued {
                        row,
                        idempotent: true,
                        superseded: None,
                    }
                } else {
                    QueueBuildLeaseResult::LeaseIdentityConflict { existing: row }
                });
            }
            sqlx::query("DELETE FROM build_leases WHERE consumer_kind=$1 AND consumer_id=$2 AND state='terminal'")
                .bind(input.key.consumer_kind.as_str())
                .bind(&input.key.consumer_id)
                .execute(&mut *tx)
                .await?;
            let reason = row
                .terminal_reason
                .clone()
                .unwrap_or_else(|| "unknown".to_owned());
            tracing::info!(
                consumer_id = %input.key.consumer_id,
                terminal_reason = %reason,
                "retired spent task_dispatch lease attempt; minting a fresh one"
            );
            superseded = Some(reason);
        }
        let row = sqlx::query_as::<_, DbRow>(&format!("INSERT INTO build_leases (consumer_kind,consumer_id,immutable_identity,queue_deadline,launch_deadline,weight,state) VALUES ($1,$2,$3,$4::timestamptz,$5::timestamptz,$6,'queued') RETURNING {COLS}"))
            .bind(input.key.consumer_kind.as_str()).bind(&input.key.consumer_id).bind(&input.immutable_identity).bind(&input.queue_deadline).bind(&input.launch_deadline).bind(input.weight).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(QueueBuildLeaseResult::Queued {
            row: row.try_into()?,
            idempotent: false,
            superseded,
        })
    }

    /// Reconcile the durable cap, then select only the oldest non-expired queue
    /// entry if all already granted/launching/bound/active/suspect units fit.
    pub async fn grant_next(
        &self,
        cap: i64,
        now: &str,
        launch_deadline: Option<&str>,
    ) -> DbResult<GrantNextBuildLeaseResult> {
        validate_cap(cap)?;
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        set_cap_tx(&mut tx, cap).await?;
        expire_queued_tx(&mut tx, now).await?;
        let occupied = occupancy_tx(&mut tx).await?;
        // Strict FIFO by weight: read the queue head, then ask whether IT fits.
        //
        // The head is never skipped in favour of a lighter row further back.
        // Skipping would leave a heavy consumer -- a warm Job whose CPU request
        // was raised above one slot -- starved indefinitely behind an unbroken
        // stream of weight-1 task dispatches, which is precisely the population
        // that arrives most often. Head-of-line blocking costs some utilisation
        // when the head does not fit and a lighter row would have; that is the
        // deliberate price of a starvation-free queue.
        //
        // A zero-weight head (the non-double-charge re-entry of an invocation
        // whose dispatch slot is already held) always fits, so it is granted
        // immediately and never queues behind capacity it already owns.
        let candidate: Option<(String, String, i64)> = sqlx::query_as("SELECT consumer_kind, consumer_id, weight FROM build_leases WHERE state='queued' AND (queue_deadline IS NULL OR queue_deadline > $1::timestamptz) ORDER BY enqueue_sequence FOR UPDATE SKIP LOCKED LIMIT 1").bind(now).fetch_optional(&mut *tx).await?;
        let Some((kind, id, weight)) = candidate else {
            tx.commit().await?;
            return Ok(GrantNextBuildLeaseResult::Empty {
                occupancy: occupied,
                cap,
            });
        };
        if occupied.saturating_add(weight) > cap {
            tx.commit().await?;
            return Ok(GrantNextBuildLeaseResult::Empty {
                occupancy: occupied,
                cap,
            });
        }
        // Coordinator-originated deadlines are durable queue data. Direct
        // repository users may still supply one at grant time, but may not
        // erase a deadline already retained on the queued row.
        let row = sqlx::query_as::<_, DbRow>(&format!("UPDATE build_leases SET state='granted', fencing_token=nextval('build_lease_fencing_token_seq'), granted_at=$1::timestamptz, launch_deadline=COALESCE($2::timestamptz, launch_deadline), updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}"))
            .bind(now).bind(launch_deadline).bind(kind).bind(id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(GrantNextBuildLeaseResult::Granted(row.try_into()?))
    }

    /// Whether this task already holds an occupying layer-1 dispatch slot.
    ///
    /// This is the durable half of the non-double-charge rule. A build-capable
    /// task-run reserves a `task_dispatch` slot before its pod exists, for
    /// exactly the compile it is expected to run; when that compile escalates
    /// to a `task_invocation` lease, charging capacity again would count one
    /// physical compile twice. The invocation is therefore weighed against this
    /// answer, not against its own role.
    ///
    /// Matched on the parsed first segment of `consumer_id` rather than a `LIKE`
    /// prefix, so a task id containing a wildcard character cannot widen the
    /// match. `split_part` mirrors exactly how `BuildLeaseService` composes the
    /// dispatch consumer id (`{task_id}:{generation}`).
    pub async fn has_occupying_dispatch(&self, task_id: &str) -> DbResult<bool> {
        self.db.ensure_initialized().await?;
        // `1::bigint`, not a bare `1`: Postgres types a bare integer literal as
        // INT4 and decoding it as i64 fails at runtime, which -- because
        // `weight_for` fails closed -- would have made EVERY invocation lease
        // permanently unavailable rather than merely mis-weighted.
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1::bigint FROM build_leases \
             WHERE consumer_kind = 'task_dispatch' \
               AND split_part(consumer_id, ':', 1) = $1 \
               AND state = ANY($2) LIMIT 1",
        )
        .bind(task_id)
        .bind(OCCUPYING.as_slice())
        .fetch_optional(self.db.pool())
        .await?;
        Ok(found.is_some())
    }

    /// Terminalize every still-QUEUED dispatch row for a task.
    ///
    /// A queued row holds a FIFO position but occupies nothing, so leaving one
    /// behind looks harmless. It is not: `grant_next` selects the oldest queued
    /// row, so an orphan whose task is gone is eventually GRANTED, occupies a
    /// slot, and is never released by anyone -- a permanent capacity leak that
    /// only shows up as a pool that mysteriously shrinks.
    ///
    /// Deliberately restricted to `queued`. An occupying row belongs to a live
    /// or recovering pod and must be released under its fencing token, never
    /// swept by task identity.
    pub async fn abandon_queued_dispatch(&self, task_id: &str) -> DbResult<u64> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let result = sqlx::query(
            "UPDATE build_leases \
             SET state='terminal', terminal_reason=$2, terminal_at=now(), updated_at=now() \
             WHERE consumer_kind='task_dispatch' \
               AND split_part(consumer_id, ':', 1) = $1 \
               AND state='queued'",
        )
        .bind(task_id)
        .bind(BuildLeaseTerminalReason::Abandoned.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn set_cap(&self, cap: i64) -> DbResult<BuildLeaseSnapshot> {
        validate_cap(cap)?;
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        set_cap_tx(&mut tx, cap).await?;
        let snapshot = snapshot_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(snapshot)
    }
    pub async fn snapshot(&self) -> DbResult<BuildLeaseSnapshot> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let snapshot = snapshot_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    /// Look up one durable row, including a retained terminal outcome. Service
    /// retries use this instead of `snapshot`, whose recovery view excludes
    /// terminal rows.
    pub async fn get(&self, key: &BuildLeaseKey) -> DbResult<Option<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, false).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Every task-invocation lease that has been fenced onto a concrete pod.
    ///
    /// `bound_pod_uid` is the ONLY durable pod-UID evidence this database holds:
    /// the schema's `build_leases_bound_uid_check` constraint permits it solely
    /// in the pod-bound and terminal states, and it is written exclusively by
    /// the fencing-token `bind` path. Run-dir reconciliation (proposal nquz)
    /// therefore reads it as authoritative ownership; every directory without a
    /// matching row stays quarantined. Read-only and lock-free: it is a startup
    /// inventory query, never part of a lease decision.
    pub async fn list_pod_bound_task_invocations(&self) -> DbResult<Vec<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {COLS} FROM build_leases \
             WHERE consumer_kind='task_invocation' AND bound_pod_uid IS NOT NULL \
             ORDER BY consumer_id"
        ))
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter().map(BuildLeaseRow::try_from).collect()
    }

    /// Atomically consume the one retry credit attached to a committed queue
    /// deadline. False covers both replay and non-timeout terminal rows.
    pub async fn consume_timeout_credit(&self, key: &BuildLeaseKey) -> DbResult<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let consumed = sqlx::query_scalar::<_, bool>(
            "UPDATE build_leases SET timeout_credit_consumed=TRUE, updated_at=now() \
             WHERE consumer_kind=$1 AND consumer_id=$2 AND state='terminal' \
             AND terminal_reason=$3 AND timeout_credit_consumed=FALSE \
             RETURNING TRUE",
        )
        .bind(key.consumer_kind.as_str())
        .bind(&key.consumer_id)
        .bind(BuildLeaseTerminalReason::DeadlineExpired.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        tx.commit().await?;
        Ok(consumed)
    }

    /// Status reports are idempotent, token-fenced and cannot resurrect terminal work.
    pub async fn status(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        state: BuildLeaseState,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        // Granting is allocator-owned: it mints the fencing token. A status
        // report therefore starts at Launching and cannot move work backwards.
        if matches!(
            state,
            BuildLeaseState::Queued | BuildLeaseState::Granted | BuildLeaseState::Terminal
        ) {
            return Err(DbError::InvalidData(
                "status must be a post-grant occupied lifecycle state".into(),
            ));
        }
        // Status reports are acknowledgements, never commands to move a lease
        // backwards. In particular, a delayed Launching acknowledgement after
        // bind must replay the bound row rather than overwrite its pod binding.
        let allowed = match state {
            BuildLeaseState::Launching => {
                &[BuildLeaseState::Granted, BuildLeaseState::Launching][..]
            }
            BuildLeaseState::Bound => &[
                BuildLeaseState::Granted,
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
            ][..],
            BuildLeaseState::Active => &[
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
                BuildLeaseState::Active,
                // A complete retry may clear an uncertain Kubernetes
                // observation after the immutable binding is re-confirmed.
                BuildLeaseState::Suspect,
            ][..],
            BuildLeaseState::Suspect => &[
                BuildLeaseState::Granted,
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
                BuildLeaseState::Active,
                BuildLeaseState::Suspect,
            ][..],
            BuildLeaseState::Queued | BuildLeaseState::Granted | BuildLeaseState::Terminal => {
                unreachable!("validated above")
            }
        };
        self.transition(key, token, allowed, state, None, cleanup)
            .await
    }
    /// Atomically abandon only an ungranted queued request. Terminal rows are
    /// replayed so a lost abandon response remains idempotent.
    pub async fn abandon_queued(
        &self,
        key: &BuildLeaseKey,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state != BuildLeaseState::Queued {
            return Err(DbError::InvalidTransition(
                "cannot abandon occupied build lease".into(),
            ));
        }
        let result = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_leases SET state='terminal',terminal_reason=$4,candidate_cleanup=COALESCE($1, candidate_cleanup),terminal_at=now(),updated_at=now() WHERE consumer_kind=$2 AND consumer_id=$3 RETURNING {COLS}"
        ))
        .bind(cleanup.map(sqlx::types::Json))
        .bind(key.consumer_kind.as_str())
        .bind(&key.consumer_id)
        .bind(BuildLeaseTerminalReason::Abandoned.as_str())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        result.try_into()
    }
    pub async fn cancel(
        &self,
        key: &BuildLeaseKey,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(key, None, BuildLeaseTerminalReason::Cancelled, cleanup)
            .await
    }
    /// Fenced counterpart for a cancel request that carries a grant token.
    pub async fn cancel_fenced(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(
            key,
            Some(token),
            BuildLeaseTerminalReason::Cancelled,
            cleanup,
        )
        .await
    }
    pub async fn release(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(
            key,
            Some(token),
            BuildLeaseTerminalReason::Released,
            cleanup,
        )
        .await
    }

    /// Bind is the sole operation which can set a pod UID; the database trigger
    /// additionally makes that UID permanently immutable.
    pub async fn bind(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        pod_uid: &str,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        if pod_uid.trim().is_empty() {
            return Err(DbError::InvalidData("pod UID must not be blank".into()));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if row.fencing_token != Some(token) {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
        }
        if row
            .bound_pod_uid
            .as_deref()
            .is_some_and(|uid| uid != pod_uid)
        {
            return Err(DbError::InvalidTransition(
                "pod UID does not match build lease".into(),
            ));
        }
        // A delayed bind with the committed immutable pod identity replays the
        // forward row without moving Active, Suspect, or Terminal backward.
        if matches!(
            row.state,
            BuildLeaseState::Bound
                | BuildLeaseState::Active
                | BuildLeaseState::Suspect
                | BuildLeaseState::Terminal
        ) && row.bound_pod_uid.as_deref() == Some(pod_uid)
        {
            tx.commit().await?;
            return Ok(row);
        }
        if !matches!(
            row.state,
            BuildLeaseState::Granted | BuildLeaseState::Launching
        ) {
            return Err(DbError::InvalidTransition(format!(
                "cannot bind build lease from {:?}",
                row.state
            )));
        }
        let result = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_leases SET state='bound',bound_pod_uid=$1,candidate_cleanup=COALESCE($2,candidate_cleanup),updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}"
        ))
        .bind(pod_uid)
        .bind(cleanup.map(sqlx::types::Json))
        .bind(key.consumer_kind.as_str())
        .bind(&key.consumer_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        result.try_into()
    }
    pub async fn expire_deadlines(&self, now: &str) -> DbResult<Vec<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let rows=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state='terminal', terminal_reason=$2, terminal_at=$1::timestamptz, updated_at=now() WHERE state='queued' AND queue_deadline <= $1::timestamptz RETURNING {COLS}")).bind(now).bind(BuildLeaseTerminalReason::DeadlineExpired.as_str()).fetch_all(&mut *tx).await?;
        let launch=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state='suspect', updated_at=now() WHERE state IN ('granted','launching') AND launch_deadline <= $1::timestamptz RETURNING {COLS}")).bind(now).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        rows.into_iter()
            .chain(launch)
            .map(TryInto::try_into)
            .collect()
    }

    /// Every currently nonterminal lease, each paired with whether it has been
    /// untouched for at least `settle_seconds`.
    ///
    /// Read-only and lock-free: reclamation gathers evidence from this listing
    /// and then fences its write on the row's `updated_at`, so a lease that
    /// moves between the listing and the write is rejected rather than raced.
    ///
    /// Scoped to [`NONTERMINAL`] rather than [`OCCUPYING`] on purpose: the
    /// reclaimer is the ledger's only self-healing path, and the fence that
    /// blocks the authority cutover counts every nonterminal row. A sweep that
    /// could not even SEE a `queued` row could never retire one.
    pub async fn list_nonterminal_with_settlement(
        &self,
        settle_seconds: i64,
    ) -> DbResult<Vec<(BuildLeaseRow, bool)>> {
        if settle_seconds < 0 {
            return Err(DbError::InvalidData(
                "settle window must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, SettlementDbRow>(&format!(
            "SELECT {COLS}, (updated_at <= now() - make_interval(secs => $2)) AS settled \
             FROM build_leases WHERE state = ANY($1) ORDER BY enqueue_sequence"
        ))
        .bind(NONTERMINAL.as_slice())
        .bind(settle_seconds as f64)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                let settled = row.settled.unwrap_or(false);
                BuildLeaseRow::try_from(row.row).map(|row| (row, settled))
            })
            .collect()
    }

    /// Retire one occupying lease whose Kubernetes object is provably gone.
    ///
    /// Nothing else in this ledger can leave an occupying state without a call
    /// from the process that holds the lease: `release`/`cancel` need the
    /// fencing token, `abandon_queued` refuses anything past `queued`, and
    /// `expire_deadlines` only downgrades `granted`/`launching` to the equally
    /// occupying `suspect`. A lease whose holder died — or, the production
    /// shape this fixes, a `granted` row whose requester gave up before it was
    /// ever acknowledged — therefore occupies the shared cap forever and
    /// starves every later build. This is the durable half of the reconciler
    /// that retires it; the caller owns the Kubernetes absence evidence.
    ///
    /// The write is a compare-and-set on the full observed identity (state,
    /// immutable identity, fencing token, bound pod UID, and `updated_at`).
    /// Anything that changed after the proof was gathered yields
    /// [`ReclaimAbsentBuildLeaseOutcome::Fenced`] and writes nothing, so this
    /// can never retire a lease that might still correspond to live work.
    pub async fn reclaim_absent_object(
        &self,
        input: &ReclaimAbsentBuildLeaseInput,
    ) -> DbResult<ReclaimAbsentBuildLeaseOutcome> {
        if !NONTERMINAL.contains(&state_str(input.observed_state)) {
            return Err(DbError::InvalidData(
                "reclamation evidence must describe a nonterminal build lease state".into(),
            ));
        }
        if input.terminal_reason == BuildLeaseTerminalReason::OperatorCleared {
            return Err(DbError::InvalidData(
                "operator_cleared is not a reclamation proof; use clear_for_operator".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let Some(row) = fetch(&mut tx, &input.key, true).await? else {
            tx.commit().await?;
            return Ok(ReclaimAbsentBuildLeaseOutcome::Fenced {
                reason: "build lease no longer exists".into(),
            });
        };
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(ReclaimAbsentBuildLeaseOutcome::AlreadyTerminal(row));
        }
        if row.state != input.observed_state
            || row.immutable_identity != input.observed_immutable_identity
            || row.fencing_token != input.observed_fencing_token
            || row.bound_pod_uid != input.observed_bound_pod_uid
            || row.updated_at != input.observed_updated_at
        {
            let reason = format!(
                "build lease changed after the absence proof (observed {:?}@{}, found {:?}@{})",
                input.observed_state, input.observed_updated_at, row.state, row.updated_at
            );
            tx.commit().await?;
            return Ok(ReclaimAbsentBuildLeaseOutcome::Fenced { reason });
        }
        let reclaimed = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_leases SET state='terminal',terminal_reason=$3,\
             terminal_at=now(),updated_at=now() WHERE consumer_kind=$1 AND consumer_id=$2 \
             RETURNING {COLS}"
        ))
        .bind(input.key.consumer_kind.as_str())
        .bind(&input.key.consumer_id)
        .bind(input.terminal_reason.as_str())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ReclaimAbsentBuildLeaseOutcome::Reclaimed(
            reclaimed.try_into()?,
        ))
    }

    /// Every nonterminal lease, oldest first. The operator read behind
    /// `djinn-server build-lease list`.
    pub async fn list_nonterminal(&self) -> DbResult<Vec<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {COLS} FROM build_leases WHERE state = ANY($1) ORDER BY enqueue_sequence"
        ))
        .bind(NONTERMINAL.as_slice())
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    /// Retire nonterminal leases on an operator's explicit instruction.
    ///
    /// This is the sanctioned answer to `preflight.sh`'s "stale occupying rows
    /// must be explicitly cleared", which until now named a remedy that did not
    /// exist in any CLI, MCP tool, or admin route — leaving a hand-written SQL
    /// `UPDATE` against production as the only way out.
    ///
    /// Deliberately UNFENCED on Kubernetes evidence, and deliberately NOT
    /// reachable from any automatic path: it takes the same advisory lock as
    /// every other mutation, refuses rows that are already terminal, and stamps
    /// [`BuildLeaseTerminalReason::OperatorCleared`] so the ledger records that
    /// a human, not a proof, ended these leases. `keys` is explicit — there is
    /// no "clear everything" spelling here, because the caller that wants one
    /// can list first and pass what it read.
    pub async fn clear_for_operator(&self, keys: &[BuildLeaseKey]) -> DbResult<Vec<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let mut cleared = Vec::new();
        for key in keys {
            let Some(row) = fetch(&mut tx, key, true).await? else {
                continue;
            };
            if row.state == BuildLeaseState::Terminal {
                continue;
            }
            let updated = sqlx::query_as::<_, DbRow>(&format!(
                "UPDATE build_leases SET state='terminal',terminal_reason=$3,\
                 terminal_at=now(),updated_at=now() WHERE consumer_kind=$1 AND consumer_id=$2 \
                 RETURNING {COLS}"
            ))
            .bind(key.consumer_kind.as_str())
            .bind(&key.consumer_id)
            .bind(BuildLeaseTerminalReason::OperatorCleared.as_str())
            .fetch_one(&mut *tx)
            .await?;
            cleared.push(updated.try_into()?);
        }
        tx.commit().await?;
        Ok(cleared)
    }

    async fn terminal(
        &self,
        key: &BuildLeaseKey,
        token: Option<i64>,
        reason: BuildLeaseTerminalReason,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if token.is_some() && row.fencing_token != token {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
        }
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(row);
        }
        let result=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state='terminal',terminal_reason=$1,candidate_cleanup=COALESCE($2, candidate_cleanup),terminal_at=now(),updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}")).bind(reason.as_str()).bind(cleanup.map(sqlx::types::Json)).bind(key.consumer_kind.as_str()).bind(&key.consumer_id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        result.try_into()
    }
    async fn transition(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        allowed: &[BuildLeaseState],
        state: BuildLeaseState,
        pod_uid: Option<&str>,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if row.fencing_token != Some(token) {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
        }
        // Close the race between a service replay read and this locked
        // transition. A delayed grant acknowledgement requests Launching, but
        // any later durable state is already its successful forward outcome.
        if state == BuildLeaseState::Launching
            && matches!(
                row.state,
                BuildLeaseState::Bound
                    | BuildLeaseState::Active
                    | BuildLeaseState::Suspect
                    | BuildLeaseState::Terminal
            )
        {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state == state
            && cleanup.is_none()
            && (pod_uid.is_none() || row.bound_pod_uid.as_deref() == pod_uid)
        {
            tx.commit().await?;
            return Ok(row);
        }
        if !allowed.contains(&row.state) {
            return Err(DbError::InvalidTransition(format!(
                "cannot transition build lease from {:?}",
                row.state
            )));
        }
        if row.bound_pod_uid.is_some()
            && pod_uid.is_some()
            && row.bound_pod_uid.as_deref() != pod_uid
        {
            return Err(DbError::InvalidTransition(
                "pod UID does not match build lease".into(),
            ));
        }
        let state = match state {
            BuildLeaseState::Launching => "launching",
            BuildLeaseState::Bound => "bound",
            BuildLeaseState::Active => "active",
            BuildLeaseState::Suspect => "suspect",
            _ => unreachable!(),
        };
        let result=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state=$1,bound_pod_uid=COALESCE($2,bound_pod_uid),candidate_cleanup=COALESCE($3,candidate_cleanup),updated_at=now() WHERE consumer_kind=$4 AND consumer_id=$5 RETURNING {COLS}")).bind(state).bind(pod_uid).bind(cleanup.map(sqlx::types::Json)).bind(key.consumer_kind.as_str()).bind(&key.consumer_id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        result.try_into()
    }
}

/// The single projection every read and `RETURNING` clause of this repository
/// uses.
///
/// Every `timestamptz` is rendered as RFC3339 in UTC with millisecond
/// precision, which is the *only* timestamp representation this repository
/// emits or accepts: callers bind RFC3339 in (`$n::timestamptz`) and read
/// RFC3339 back out, so one parser round-trips the values.
///
/// A plain `::text` cast would instead yield PostgreSQL's own output format
/// (`2026-07-25 20:30:00+00`, space-separated, no `T`). That is not RFC3339,
/// and the coordinator's `build_lease::ms` — the sole reader of the echoed
/// deadlines — parses RFC3339 and maps a parse failure to `0`. Since `0` is
/// itself meaningful in that contract ("no deadline"), the cast silently turned
/// every real deadline into "unbounded" on the way out. Keep the rendering
/// here; do not add a second parser on the Rust side.
const COLS: &str = "consumer_kind,consumer_id,immutable_identity,enqueue_sequence,fencing_token,state,\
    to_char(queue_deadline AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS queue_deadline,\
    to_char(launch_deadline AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS launch_deadline,\
    bound_pod_uid,candidate_cleanup,terminal_reason,weight,timeout_credit_consumed,\
    to_char(created_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at,\
    to_char(updated_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS updated_at,\
    to_char(granted_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS granted_at,\
    to_char(terminal_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS terminal_at";
#[derive(sqlx::FromRow)]
struct SettlementDbRow {
    #[sqlx(flatten)]
    row: DbRow,
    settled: Option<bool>,
}
#[derive(sqlx::FromRow)]
struct DbRow {
    consumer_kind: String,
    consumer_id: String,
    immutable_identity: String,
    enqueue_sequence: i64,
    fencing_token: Option<i64>,
    state: String,
    queue_deadline: Option<String>,
    launch_deadline: Option<String>,
    bound_pod_uid: Option<String>,
    candidate_cleanup: Option<sqlx::types::Json<serde_json::Value>>,
    terminal_reason: Option<String>,
    weight: i64,
    timeout_credit_consumed: bool,
    created_at: String,
    updated_at: String,
    granted_at: Option<String>,
    terminal_at: Option<String>,
}
impl TryFrom<DbRow> for BuildLeaseRow {
    type Error = DbError;
    fn try_from(v: DbRow) -> DbResult<Self> {
        Ok(Self {
            key: BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::parse(&v.consumer_kind)?,
                consumer_id: v.consumer_id,
            },
            immutable_identity: v.immutable_identity,
            enqueue_sequence: v.enqueue_sequence,
            fencing_token: v.fencing_token,
            state: BuildLeaseState::parse(&v.state)?,
            queue_deadline: v.queue_deadline,
            launch_deadline: v.launch_deadline,
            bound_pod_uid: v.bound_pod_uid,
            weight: v.weight,
            candidate_cleanup: v.candidate_cleanup.map(|v| v.0),
            terminal_reason: v.terminal_reason,
            timeout_credit_consumed: v.timeout_credit_consumed,
            created_at: v.created_at,
            updated_at: v.updated_at,
            granted_at: v.granted_at,
            terminal_at: v.terminal_at,
        })
    }
}
/// Durable column spelling of a lifecycle state, so occupancy membership is
/// decided against the same [`OCCUPYING`] list the SQL uses.
fn state_str(state: BuildLeaseState) -> &'static str {
    match state {
        BuildLeaseState::Queued => "queued",
        BuildLeaseState::Granted => "granted",
        BuildLeaseState::Launching => "launching",
        BuildLeaseState::Bound => "bound",
        BuildLeaseState::Active => "active",
        BuildLeaseState::Suspect => "suspect",
        BuildLeaseState::Terminal => "terminal",
    }
}
async fn lock(tx: &mut Transaction<'_, Postgres>) -> DbResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('build-lease-ledger',0))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn fetch(
    tx: &mut Transaction<'_, Postgres>,
    key: &BuildLeaseKey,
    for_update: bool,
) -> DbResult<Option<BuildLeaseRow>> {
    let suffix = if for_update { " FOR UPDATE" } else { "" };
    let row = sqlx::query_as::<_, DbRow>(&format!(
        "SELECT {COLS} FROM build_leases WHERE consumer_kind=$1 AND consumer_id=$2{suffix}"
    ))
    .bind(key.consumer_kind.as_str())
    .bind(&key.consumer_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}
/// Occupied capacity, in build slots.
///
/// This is a SUM over `weight`, not a COUNT of rows. A zero-weight row is a
/// real occupant with a real fencing token that contributes no capacity -- see
/// [`QueueBuildLeaseInput::weight`] for why the non-double-charge rule needs
/// that. This one expression is now the ONLY capacity accounting in the system;
/// the v0 admission journal deliberately no longer computes one.
async fn occupancy_tx(tx: &mut Transaction<'_, Postgres>) -> DbResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(SUM(weight),0)::bigint FROM build_leases WHERE state=ANY($1)",
    )
    .bind(OCCUPYING.as_slice())
    .fetch_one(&mut **tx)
    .await?)
}
async fn set_cap_tx(tx: &mut Transaction<'_, Postgres>, cap: i64) -> DbResult<()> {
    sqlx::query("UPDATE build_lease_caps SET cap=$1,updated_at=now() WHERE singleton=true")
        .bind(cap)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn expire_queued_tx(tx: &mut Transaction<'_, Postgres>, now: &str) -> DbResult<()> {
    sqlx::query("UPDATE build_leases SET state='terminal',terminal_reason=$2,terminal_at=$1::timestamptz,updated_at=now() WHERE state='queued' AND queue_deadline <= $1::timestamptz").bind(now).bind(BuildLeaseTerminalReason::DeadlineExpired.as_str()).execute(&mut **tx).await?;
    Ok(())
}
async fn snapshot_tx(tx: &mut Transaction<'_, Postgres>) -> DbResult<BuildLeaseSnapshot> {
    let cap = sqlx::query_scalar("SELECT cap FROM build_lease_caps WHERE singleton=true")
        .fetch_one(&mut **tx)
        .await?;
    let occupied = occupancy_tx(tx).await?;
    let rows = sqlx::query_as::<_, DbRow>(&format!(
        "SELECT {COLS} FROM build_leases WHERE state <> 'terminal' ORDER BY enqueue_sequence"
    ))
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect::<DbResult<_>>()?;
    Ok(BuildLeaseSnapshot {
        cap,
        occupied,
        rows,
    })
}
fn validate_cap(cap: i64) -> DbResult<()> {
    if cap < 0 {
        Err(DbError::InvalidData(
            "build lease cap must be non-negative".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "build_lease_tests.rs"]
mod tests;
