//! The bounded incomplete-evidence hold (proposal `nafu`, wave 5).
//!
//! When a lane's check enumeration comes back **recoverably** incomplete — a
//! run still in progress, a check suite not yet reported — the right answer is
//! neither to route nor to drop the observation. It is to hold and poll again.
//! An unbounded hold is a silent stall, so the hold is bounded: after
//! [`CI_INCOMPLETE_HOLD_MAX_POLLS`] consecutive incomplete polls the streak
//! escalates **once**, and that escalation inserts exactly one diagnose-only
//! run-absent Tier-2 route.
//!
//! # Why this is not a counter column
//!
//! Everything difficult here is ordering, and none of it is visible if you
//! think of the streak as "a number the poller increments".
//!
//! Polls overlap. Two coordinator ticks, or a restart racing a live poll, can
//! both be enumerating the same lane at once, and a provider enumeration takes
//! seconds. **No transaction may span that call** — holding a row lock across a
//! provider round trip is how a slow GitHub becomes a database outage. So one
//! poll is *two* short transactions with the provider call in the gap:
//!
//! | | transaction | what it does |
//! | --- | --- | --- |
//! | before | [`CiIncompleteHoldRepository::reserve_poll`] | locks or creates the streak, assigns this poll a **sequence**, records the child observation, commits |
//! | *(gap)* | — | the caller enumerates the provider |
//! | after | [`CiIncompleteHoldRepository::apply_poll`] | locks the streak again, revalidates, applies **at most one** outcome |
//!
//! Once there is a gap, *arrival order is meaningless*: the poll that started
//! first can finish last. So arrival order is not the authority — the reserved
//! sequence is. `apply_poll` compares the child's reserved `poll_sequence`
//! against the streak's retained `last_applied_poll_sequence` and applies only
//! when it is **strictly greater**. A late poll's stale answer is
//! `Superseded`: it neither increments the streak nor resets it, and it writes
//! nothing but its own `superseded_observation` marker.
//!
//! Two concrete bugs that ordering rule prevents, both of which a plain counter
//! has:
//!
//! * two concurrent pollers of one still-incomplete lane each increment, so the
//!   bound is reached in half the polls it claims to allow; and
//! * a late **incomplete** answer landing after an on-time **complete** one
//!   resurrects a streak that had legitimately reset — the lane is green and
//!   the hold escalates anyway.
//!
//! # What is retained, and why nothing is deleted
//!
//! A complete enumeration resets `poll_count` to zero and clears
//! `escalated_at`, but **the row and the high-watermark survive**. Deleting the
//! streak would look tidier and would silently re-admit every in-flight
//! observation that had already been overtaken: with the watermark gone, the
//! next stale apply would find a fresh row at sequence zero and count itself.
//! "Retained high-watermark" is the property the whole contract rests on, which
//! is why migration 195 enforces it with a `BEFORE UPDATE` trigger as well —
//! an invariant upheld only by the code that happens to write it is a
//! convention, not a guarantee.
//!
//! # Idempotency across a restart
//!
//! `reserve_poll` is keyed by the caller's `poll_observation_id`. A retry of
//! the **same logical poll** reads back the sequence it was already assigned
//! and reserves no second one ([`CiHoldReservation::replayed`]). Without that,
//! a crash-loop between reserve and apply would burn sequence space and — far
//! worse — make every earlier in-flight observation look superseded.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};

use super::ci_route_attempt::{
    CiAction, CiLane, CiRouteReservation, CiRouteSubject, CiSubjectKind, CiTier2Reason,
};
use crate::Result;
use crate::database::Database;
use crate::error::DbError;

/// Consecutive incomplete polls a hold may accumulate before it escalates.
///
/// The increment that *reaches* this number escalates; it is a ceiling, not a
/// threshold that must be exceeded.
pub const CI_INCOMPLETE_HOLD_MAX_POLLS: i64 = 12;

/// `WHERE` fragment naming one streak by its full identity.
///
/// `dequeue_id IS NOT DISTINCT FROM $7` rather than `=`: the PR-head lane has
/// no dequeue id, and `NULL = NULL` would match nothing, so every PR-head poll
/// would fail to find its own streak and mint a new one — and the bound would
/// never be reached. This is the query-side twin of the index's
/// `NULLS NOT DISTINCT`.
const BY_IDENTITY: &str = "subject_kind = $1 AND subject_id = $2 AND repository_id = $3 \
     AND pr_number = $4 AND pr_head_sha = $5 AND lane = $6 \
     AND dequeue_id IS NOT DISTINCT FROM $7";

const STREAK_COLUMNS: &str = "id, subject_kind, subject_id, repository_id, pr_number, pr_head_sha, lane, dequeue_id, \
    next_poll_sequence, last_applied_poll_sequence, poll_count, \
    to_char(escalated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS escalated_at, \
    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at, \
    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS updated_at";

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The thing a hold streak counts polls for.
///
/// `(repository_id, pr_number, pr_head_sha, lane, dequeue_id)` per the
/// proposal, scoped additionally by the route subject like every other relation
/// in this feature.
///
/// **`repository_id` is `owner/repo`, which is mutable.** A repository rename
/// mid-flight starts a fresh streak with a zero count. That is safe *here* in a
/// way it would not be on `ci_route_attempts`: a streak only authorizes
/// **counting**, never a provider call, so a duplicated streak costs at most a
/// few extra polls before the bound is reached again. A route row carries a
/// provider-call authorization and could not tolerate the same laxity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiHoldIdentity {
    pub subject: CiRouteSubject,
    /// `owner/repo`. See the type note on mutability.
    pub repository_id: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
    pub lane: CiLane,
    /// Merge-group dequeue event identity. `None` for the PR-head lane.
    pub dequeue_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One durable hold streak.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiHoldStreak {
    pub id: String,
    pub identity: CiHoldIdentity,
    /// The reservation counter. Every reserved sequence is one greater than the
    /// previous value of this column.
    pub next_poll_sequence: i64,
    /// The retained high-watermark. An observation whose reserved sequence is
    /// `<=` this has been overtaken and applies nothing.
    pub last_applied_poll_sequence: i64,
    /// Consecutive incomplete polls. Reset by a complete enumeration; the row
    /// and the watermark survive that reset.
    pub poll_count: i64,
    /// Set exactly once, in the same transaction as the increment that reached
    /// [`CI_INCOMPLETE_HOLD_MAX_POLLS`] and as the route insert it authorizes.
    pub escalated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl CiHoldStreak {
    /// Whether this streak has already escalated.
    #[must_use]
    pub fn has_escalated(&self) -> bool {
        self.escalated_at.is_some()
    }

    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            identity: CiHoldIdentity {
                subject: CiRouteSubject {
                    kind: CiSubjectKind::parse(row.try_get::<String, _>("subject_kind")?.as_str())?,
                    id: row.try_get("subject_id")?,
                },
                repository_id: row.try_get("repository_id")?,
                pr_number: row.try_get("pr_number")?,
                pr_head_sha: row.try_get("pr_head_sha")?,
                lane: CiLane::parse(row.try_get::<String, _>("lane")?.as_str())?,
                dequeue_id: row.try_get("dequeue_id")?,
            },
            next_poll_sequence: row.try_get("next_poll_sequence")?,
            last_applied_poll_sequence: row.try_get("last_applied_poll_sequence")?,
            poll_count: row.try_get("poll_count")?,
            escalated_at: row.try_get("escalated_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// One recorded poll observation, including the ones that applied nothing.
///
/// The losers are the point. An ordering contract whose superseded
/// observations leave no trace cannot be audited, and "the late poll did not
/// count" is exactly the claim a reader needs to check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CiHoldObservation {
    pub poll_observation_id: String,
    pub streak_id: String,
    pub poll_sequence: i64,
    pub reserved_at: String,
    pub applied_at: Option<String>,
    /// One of `applied_complete`, `applied_incomplete`, `escalated`,
    /// `superseded_observation`, `identity_advanced`; `None` while the poll is
    /// still in flight.
    pub apply_outcome: Option<String>,
}

// ---------------------------------------------------------------------------
// Inputs and outcomes
// ---------------------------------------------------------------------------

/// What [`CiIncompleteHoldRepository::reserve_poll`] granted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiHoldReservation {
    /// This poll's place in the authority order. Strictly increasing per
    /// streak, and never zero.
    pub poll_sequence: i64,
    pub streak_id: String,
    /// `true` when this `poll_observation_id` had already reserved a sequence
    /// and this call read it back rather than reserving a second one. A retry
    /// after a crash is a replay, not a new poll.
    pub replayed: bool,
}

/// Everything needed to insert the one diagnose-only run-absent Tier-2 route an
/// escalating hold produces, **inside the same transaction** that sets
/// `escalated_at`.
///
/// It is passed in rather than derived here because the reservation keys
/// (`provider_action_key`, the budget keys, the lease key) are the
/// coordinator's contract, not the repository's — the same division as
/// [`super::ci_route_attempt::CiRouteAttemptRepository::open_tier2_lease`].
///
/// Two fields are validated at the API boundary and rejected with
/// [`DbError::InvalidData`], because either would fabricate an identity the
/// rest of the feature has spent a migration abolishing:
///
/// * `reservation.identity.run_id` must be `None` — a hold escalates precisely
///   because the enumeration never resolved a run; and
/// * `reservation.action` must be [`CiAction::AskLead`] — the escalation
///   authorizes an adjudication, never a provider mutation.
#[derive(Clone, Debug)]
pub struct CiHoldEscalationRoute {
    pub reservation: CiRouteReservation,
    pub tier2_lease_id: String,
    pub tier2_lease_key: String,
    pub tier2_reason: CiTier2Reason,
}

/// The single outcome one [`CiIncompleteHoldRepository::apply_poll`] produced.
///
/// Exactly one of these happens per apply, and every variant except the first
/// three names the state change it made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CiHoldApply {
    /// The observation's sequence did not exceed the retained high-watermark:
    /// a newer poll already applied. **Nothing** was written except the child's
    /// `superseded_observation` marker — no count change, no watermark
    /// movement, no escalation, no reset.
    Superseded,
    /// The identity advanced (head, lane, or dequeue moved) between reserving
    /// this poll and applying it. A no-op: the old head's streak is neither
    /// incremented nor escalated, because escalating it would create a
    /// diagnose route for a head nobody is on any more.
    IdentityAdvanced,
    /// Complete evidence. The high-watermark advanced, `poll_count` reset to
    /// zero, `escalated_at` cleared — and the row **retained**.
    Reset { poll_sequence: i64 },
    /// Recoverably incomplete, still below the bound. The high-watermark
    /// advanced and the count incremented.
    Held { poll_count: i64 },
    /// The increment reached [`CI_INCOMPLETE_HOLD_MAX_POLLS`]. `escalated_at`
    /// was set and the diagnose-only route inserted **in this same
    /// transaction**, so the marker and the route commit or roll back together.
    Escalated {
        poll_count: i64,
        /// `false` when a run-absent route for this identity already existed —
        /// e.g. a `MAX_PAGES` truncation routed the same identity directly
        /// earlier. That is correct, not an error: a later reason adds evidence
        /// but opens no second lease and no second Lead session.
        route_inserted: bool,
    },
    /// The streak had already escalated. The high-watermark still advances —
    /// so this observation cannot be replayed — but the count neither
    /// increments nor dispatches anything.
    AlreadyEscalated,
    /// No streak for that identity, or no observation with that id. Never a
    /// side effect.
    NotFound,
}

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

/// Durable state for the bounded incomplete-evidence hold.
///
/// **No method here holds a connection across a provider call.**
/// [`Self::reserve_poll`] and [`Self::apply_poll`] are two independent short
/// transactions, and the caller's provider I/O happens between them with
/// nothing checked out.
pub struct CiIncompleteHoldRepository {
    db: Database,
}

impl CiIncompleteHoldRepository {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// **Short transaction #1**, run *before* provider enumeration.
    ///
    /// Locks or inserts the identity row (`SELECT ... FOR UPDATE`), increments
    /// `next_poll_sequence`, and inserts the child observation keyed by
    /// `poll_observation_id` with the assigned sequence. Commits before the
    /// caller touches the provider.
    ///
    /// A retry of the **same** logical poll id reads back its already-assigned
    /// sequence and reserves no second one ([`CiHoldReservation::replayed`] is
    /// `true`). That is what makes a crash between reserve and apply harmless.
    ///
    /// # Errors
    ///
    /// Database failures, or a `poll_observation_id` already reserved against a
    /// *different* streak — which means the caller reused an id across
    /// identities, and silently proceeding would apply one lane's poll to
    /// another's count.
    pub async fn reserve_poll(
        &self,
        identity: &CiHoldIdentity,
        poll_observation_id: &str,
    ) -> Result<CiHoldReservation> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let streak_id = lock_or_create_streak(&mut tx, identity).await?;

        // Replay check happens under the streak lock, so two retries of one
        // poll id serialize and the loser reads the winner's sequence rather
        // than colliding on the child's primary key.
        if let Some((existing_streak, sequence)) =
            existing_observation(&mut tx, poll_observation_id).await?
        {
            tx.commit().await?;
            if existing_streak != streak_id {
                return Err(DbError::InvalidData(format!(
                    "poll observation `{poll_observation_id}` was reserved against streak \
                     `{existing_streak}`, not `{streak_id}`: a poll id belongs to one identity"
                )));
            }
            return Ok(CiHoldReservation {
                poll_sequence: sequence,
                streak_id,
                replayed: true,
            });
        }

        let poll_sequence: i64 = sqlx::query_scalar(
            "UPDATE ci_incomplete_hold_streaks \
             SET next_poll_sequence = next_poll_sequence + 1, updated_at = now() \
             WHERE id = $1 RETURNING next_poll_sequence",
        )
        .bind(&streak_id)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO ci_incomplete_hold_observations \
               (poll_observation_id, streak_id, poll_sequence) VALUES ($1, $2, $3)",
        )
        .bind(poll_observation_id)
        .bind(&streak_id)
        .bind(poll_sequence)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(CiHoldReservation {
            poll_sequence,
            streak_id,
            replayed: false,
        })
    }

    /// **Short transaction #2**, run *after* provider enumeration.
    ///
    /// Locks the same identity row, revalidates it, and applies **at most one**
    /// [`CiHoldApply`] outcome. `complete` says whether the enumeration came
    /// back authoritatively complete.
    ///
    /// The checks are ordered, and the order matters:
    ///
    /// 1. `reserved != observed_current` → [`CiHoldApply::IdentityAdvanced`].
    ///    This is **before** the sequence comparison: a delayed apply for a
    ///    head that has since moved must not increment — let alone escalate —
    ///    the old head's streak, because that would create a diagnose route for
    ///    a head nobody is on any more.
    /// 2. streak or observation missing → [`CiHoldApply::NotFound`].
    /// 3. reserved sequence `<=` `last_applied_poll_sequence` →
    ///    [`CiHoldApply::Superseded`], marker only.
    /// 4. otherwise the watermark advances to this sequence and exactly one of
    ///    [`CiHoldApply::Reset`], [`CiHoldApply::AlreadyEscalated`],
    ///    [`CiHoldApply::Held`], [`CiHoldApply::Escalated`] applies.
    ///
    /// # Errors
    ///
    /// Database failures; an `escalation` whose reservation names a run or
    /// whose action is not [`CiAction::AskLead`]; or an observation that
    /// belongs to a different streak than the one this identity names.
    pub async fn apply_poll(
        &self,
        reserved: &CiHoldIdentity,
        observed_current: &CiHoldIdentity,
        poll_observation_id: &str,
        complete: bool,
        escalation: &CiHoldEscalationRoute,
    ) -> Result<CiHoldApply> {
        self.db.ensure_initialized().await?;
        validate_escalation(escalation)?;

        let mut tx = self.db.pool().begin().await?;

        // (1) The identity advanced. Marked on the child and nowhere else.
        if reserved != observed_current {
            mark_observation(&mut tx, poll_observation_id, "identity_advanced").await?;
            tx.commit().await?;
            return Ok(CiHoldApply::IdentityAdvanced);
        }

        // (2) Nothing to apply to.
        let Some(streak) = lock_streak(&mut tx, reserved).await? else {
            tx.commit().await?;
            return Ok(CiHoldApply::NotFound);
        };
        let Some((observation_streak, poll_sequence)) =
            existing_observation(&mut tx, poll_observation_id).await?
        else {
            tx.commit().await?;
            return Ok(CiHoldApply::NotFound);
        };
        if observation_streak != streak.id {
            tx.commit().await?;
            return Err(DbError::InvalidData(format!(
                "poll observation `{poll_observation_id}` belongs to streak \
                 `{observation_streak}`, not `{}`",
                streak.id
            )));
        }

        // (3) Overtaken. The single most important branch in this module: it
        // writes the marker and NOTHING else.
        if poll_sequence <= streak.last_applied_poll_sequence {
            mark_observation(&mut tx, poll_observation_id, "superseded_observation").await?;
            tx.commit().await?;
            return Ok(CiHoldApply::Superseded);
        }

        // (4) This observation is the newest applied. Everything below advances
        // the watermark to its sequence in the same statement as its effect.
        let outcome = if complete {
            advance(&mut tx, &streak.id, poll_sequence, Some(0), false).await?;
            mark_observation(&mut tx, poll_observation_id, "applied_complete").await?;
            CiHoldApply::Reset { poll_sequence }
        } else if streak.has_escalated() {
            advance(&mut tx, &streak.id, poll_sequence, None, false).await?;
            mark_observation(&mut tx, poll_observation_id, "applied_incomplete").await?;
            CiHoldApply::AlreadyEscalated
        } else {
            let poll_count = streak.poll_count + 1;
            let escalating = poll_count >= CI_INCOMPLETE_HOLD_MAX_POLLS;
            advance(
                &mut tx,
                &streak.id,
                poll_sequence,
                Some(poll_count),
                escalating,
            )
            .await?;
            if escalating {
                mark_observation(&mut tx, poll_observation_id, "escalated").await?;
                let route_inserted = insert_escalation_route(&mut tx, escalation).await?;
                CiHoldApply::Escalated {
                    poll_count,
                    route_inserted,
                }
            } else {
                mark_observation(&mut tx, poll_observation_id, "applied_incomplete").await?;
                CiHoldApply::Held { poll_count }
            }
        };

        tx.commit().await?;
        Ok(outcome)
    }

    /// Read the current streak for an identity.
    ///
    /// # Errors
    ///
    /// Database failures, or a row carrying a value outside a durable
    /// vocabulary.
    pub async fn get(&self, identity: &CiHoldIdentity) -> Result<Option<CiHoldStreak>> {
        self.db.ensure_initialized().await?;
        let row = bind_identity(
            sqlx::query(&format!(
                "SELECT {STREAK_COLUMNS} FROM ci_incomplete_hold_streaks WHERE {BY_IDENTITY}"
            )),
            identity,
        )
        .fetch_optional(self.db.pool())
        .await?;
        row.as_ref().map(CiHoldStreak::from_row).transpose()
    }

    /// Read one recorded poll observation, applied or not.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn observation(
        &self,
        poll_observation_id: &str,
    ) -> Result<Option<CiHoldObservation>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT poll_observation_id, streak_id, poll_sequence, \
                    to_char(reserved_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS reserved_at, \
                    to_char(applied_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS applied_at, \
                    apply_outcome \
             FROM ci_incomplete_hold_observations WHERE poll_observation_id = $1",
        )
        .bind(poll_observation_id)
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(CiHoldObservation {
            poll_observation_id: row.try_get("poll_observation_id")?,
            streak_id: row.try_get("streak_id")?,
            poll_sequence: row.try_get("poll_sequence")?,
            reserved_at: row.try_get("reserved_at")?,
            applied_at: row.try_get("applied_at")?,
            apply_outcome: row.try_get("apply_outcome")?,
        }))
    }
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

fn bind_identity<'q>(
    query: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    identity: &'q CiHoldIdentity,
) -> sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments> {
    query
        .bind(identity.subject.kind.as_str())
        .bind(&identity.subject.id)
        .bind(&identity.repository_id)
        .bind(identity.pr_number)
        .bind(&identity.pr_head_sha)
        .bind(identity.lane.as_str())
        .bind(identity.dequeue_id.as_deref())
}

fn bind_identity_scalar<'q, O>(
    query: sqlx::query::QueryScalar<'q, Postgres, O, sqlx::postgres::PgArguments>,
    identity: &'q CiHoldIdentity,
) -> sqlx::query::QueryScalar<'q, Postgres, O, sqlx::postgres::PgArguments> {
    query
        .bind(identity.subject.kind.as_str())
        .bind(&identity.subject.id)
        .bind(&identity.repository_id)
        .bind(identity.pr_number)
        .bind(&identity.pr_head_sha)
        .bind(identity.lane.as_str())
        .bind(identity.dequeue_id.as_deref())
}

/// Take the streak row lock and read it back, or `None` if there is no streak.
async fn lock_streak(
    tx: &mut Transaction<'_, Postgres>,
    identity: &CiHoldIdentity,
) -> Result<Option<CiHoldStreak>> {
    // Two statements rather than one: `STREAK_COLUMNS` contains `to_char`
    // expressions and Postgres refuses `FOR UPDATE` on a projection it cannot
    // prove is a plain row reference. Same shape as `ci_route_attempt`'s
    // `fetch_locked`.
    let locked: Option<String> = bind_identity_scalar(
        sqlx::query_scalar(&format!(
            "SELECT id FROM ci_incomplete_hold_streaks WHERE {BY_IDENTITY} FOR UPDATE"
        )),
        identity,
    )
    .fetch_optional(&mut **tx)
    .await?;
    if locked.is_none() {
        return Ok(None);
    }
    let row = bind_identity(
        sqlx::query(&format!(
            "SELECT {STREAK_COLUMNS} FROM ci_incomplete_hold_streaks WHERE {BY_IDENTITY}"
        )),
        identity,
    )
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref().map(CiHoldStreak::from_row).transpose()
}

/// Lock the streak for an identity, creating it at zero if it is absent.
///
/// `ON CONFLICT DO NOTHING` with **no** conflict target, so the arbiter is any
/// unique index — which is what the `NULLS NOT DISTINCT` identity index needs.
/// Two concurrent first-polls of one lane both see "absent"; one insert wins,
/// the loser inserts nothing and both then take the same row lock.
async fn lock_or_create_streak(
    tx: &mut Transaction<'_, Postgres>,
    identity: &CiHoldIdentity,
) -> Result<String> {
    if let Some(streak) = lock_streak(tx, identity).await? {
        return Ok(streak.id);
    }
    bind_identity(
        sqlx::query(
            "INSERT INTO ci_incomplete_hold_streaks \
               (id, subject_kind, subject_id, repository_id, pr_number, pr_head_sha, lane, dequeue_id) \
             VALUES ($8, $1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
        ),
        identity,
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .execute(&mut **tx)
    .await?;
    lock_streak(tx, identity)
        .await?
        .map(|s| s.id)
        .ok_or_else(|| DbError::Internal("hold streak row disappeared after insert".to_owned()))
}

/// `(streak_id, poll_sequence)` for an already-reserved observation.
async fn existing_observation(
    tx: &mut Transaction<'_, Postgres>,
    poll_observation_id: &str,
) -> Result<Option<(String, i64)>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT streak_id, poll_sequence FROM ci_incomplete_hold_observations \
         WHERE poll_observation_id = $1",
    )
    .bind(poll_observation_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row)
}

/// Stamp an observation's terminal marker, once.
///
/// `applied_at IS NULL` in the guard keeps it write-once: a duplicate apply of
/// the same observation id records nothing new, which is what makes the audit
/// trail a record of what *happened* rather than of what was last attempted.
async fn mark_observation(
    tx: &mut Transaction<'_, Postgres>,
    poll_observation_id: &str,
    outcome: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE ci_incomplete_hold_observations \
         SET applied_at = now(), apply_outcome = $2 \
         WHERE poll_observation_id = $1 AND applied_at IS NULL",
    )
    .bind(poll_observation_id)
    .bind(outcome)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Advance the high-watermark, and optionally set the count and the escalation
/// marker, in one statement.
///
/// One statement rather than three so the watermark can never move without its
/// effect, or the reverse. `poll_count` is `Some` only when it changes;
/// `escalating` stamps `escalated_at` and is never passed together with a reset.
async fn advance(
    tx: &mut Transaction<'_, Postgres>,
    streak_id: &str,
    poll_sequence: i64,
    poll_count: Option<i64>,
    escalating: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE ci_incomplete_hold_streaks \
         SET last_applied_poll_sequence = $2, \
             poll_count = COALESCE($3, poll_count), \
             escalated_at = CASE \
                 WHEN $4 THEN now() \
                 WHEN $3 = 0 THEN NULL \
                 ELSE escalated_at END, \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(streak_id)
    .bind(poll_sequence)
    .bind(poll_count)
    .bind(escalating)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn validate_escalation(escalation: &CiHoldEscalationRoute) -> Result<()> {
    if let Some(run_id) = escalation.reservation.identity.run_id {
        return Err(DbError::InvalidData(format!(
            "an escalating hold names no run, but the escalation route carries `run_id = {run_id}`: \
             a hold escalates precisely because the enumeration never resolved a run"
        )));
    }
    if escalation.reservation.action != CiAction::AskLead {
        return Err(DbError::InvalidData(format!(
            "an escalating hold authorizes an adjudication, not a provider mutation: \
             `{}` is not a valid escalation action",
            escalation.reservation.action.as_str()
        )));
    }
    Ok(())
}

/// Insert the one diagnose-only run-absent Tier-2 route, with its lease already
/// open, in the caller's transaction.
///
/// The lease columns are set **on the INSERT** rather than by a follow-up
/// `open_tier2_lease`, so the route, its lease, and the `escalated_at` marker
/// are a single atomic fact. A crash between them is not a state this schema
/// can reach.
///
/// `ON CONFLICT DO NOTHING` with no target, so every arbiter applies: the
/// primary key, the `NULLS NOT DISTINCT` evidence-identity index (a run-absent
/// route for this identity already exists), and the partial open-lease index
/// (another row on this subject already holds the head's Tier-2 hold). All
/// three mean the same thing to the caller — this identity is already
/// adjudicated or being adjudicated, so opening a second lease and spending a
/// second Lead session would be the bug.
async fn insert_escalation_route(
    tx: &mut Transaction<'_, Postgres>,
    escalation: &CiHoldEscalationRoute,
) -> Result<bool> {
    let input = &escalation.reservation;
    let inserted = sqlx::query(
        "INSERT INTO ci_route_attempts \
           (subject_kind, subject_id, provider_action_key, lane, pr_number, pr_head_sha, \
            run_id, run_head_sha, dequeue_id, origin_state, class, action, \
            transient_fingerprint, retry_budget_key, head_budget_key, action_phase, \
            tier2_lease_id, tier2_lease_key, tier2_lease_state, tier2_lease_reason, tier2_leased_at) \
         VALUES ($1,$2,$3,$4,$5,$6,NULL,$7,$8,$9,$10,$11,$12,$13,$14,'reserved', \
                 $15,$16,'open',$17,now()) \
         ON CONFLICT DO NOTHING",
    )
    .bind(input.subject.kind.as_str())
    .bind(&input.subject.id)
    .bind(&input.provider_action_key)
    .bind(input.identity.lane.as_str())
    .bind(input.identity.pr_number)
    .bind(&input.identity.pr_head_sha)
    .bind(&input.identity.run_head_sha)
    .bind(input.identity.dequeue_id.as_deref())
    .bind(input.origin_state.as_str())
    .bind(input.class.as_str())
    .bind(input.action.as_str())
    .bind(&input.transient_fingerprint)
    .bind(&input.retry_budget_key)
    .bind(&input.head_budget_key)
    .bind(&escalation.tier2_lease_id)
    .bind(&escalation.tier2_lease_key)
    .bind(escalation.tier2_reason.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(inserted.rows_affected() == 1)
}
