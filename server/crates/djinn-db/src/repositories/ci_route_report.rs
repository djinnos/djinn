//! CI evidence-routing reporting and the rollback quiescence report
//! (proposal `nafu`, wave 5).
//!
//! Everything here is a **read** over the tables wave 1 created, plus one
//! append-only write: the rollback quiescence report, which is the single
//! repository-checkable row the proposal requires before a binary rollback.
//!
//! # The two traps this module exists to avoid
//!
//! **A reopen can live in either of two columns.** Terminalization is
//! write-once. A route that already terminalized on its provider result —
//! `action_failed` or `outcome_unknown` — keeps that outcome and records the
//! Lead adjudication in `tier2_resolution` instead. So a report that counts
//! reopens with `WHERE terminal_outcome = 'repair_reopened'` silently drops
//! **every reopen that followed a provider failure**, which is precisely the
//! population an operator is looking for when they ask why CI routing is
//! spending worker sessions. Every adjudication count below reads
//! `COALESCE(tier2_resolution, terminal_outcome)` — the SQL spelling of
//! [`CiRouteAttempt::adjudicated_outcome`].
//!
//! **A fallback wears the route's own reason.** The supervisor derives a
//! rejected result's `diagnostic_reason` from the route's Tier-2 reason, so a
//! Lead *timeout* on a `causal_failure` route is durably stamped
//! `no_grounded_remedy` — the same spelling a Lead that answered and found no
//! remedy produces. Counting diagnostic reasons alone therefore conflates
//! "Lead diagnosed" with "Lead never answered". [`CiRouteReport`] splits them
//! on `lead_rejection`, which wave 5 added for exactly this reason.
//!
//! [`CiRouteAttempt::adjudicated_outcome`]: super::ci_route_attempt::CiRouteAttempt::adjudicated_outcome

use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::ci_route_attempt::{CiCallingRecoveryReason, CiLane, CiRouteAttemptRepository};
use crate::{Error as DbError, Result};

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// Lane and time-window scoping for a report.
///
/// Both bounds are RFC3339 strings compared against `reserved_at`, i.e. when
/// the route was *created*, not when it terminalized. A window keyed on
/// terminalization would drop every route that is still in flight, and "how
/// many routes are still open in this window" is one of the questions the
/// report exists to answer.
#[derive(Clone, Debug, Default)]
pub struct CiRouteReportFilter {
    pub lane: Option<CiLane>,
    pub since: Option<String>,
    pub until: Option<String>,
}

impl CiRouteReportFilter {
    /// Everything, unscoped.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Scope to one lane.
    #[must_use]
    pub fn lane(mut self, lane: CiLane) -> Self {
        self.lane = Some(lane);
        self
    }

    /// Scope to routes reserved at or after `since` (RFC3339).
    #[must_use]
    pub fn since(mut self, since: impl Into<String>) -> Self {
        self.since = Some(since.into());
        self
    }

    /// Scope to routes reserved strictly before `until` (RFC3339).
    #[must_use]
    pub fn until(mut self, until: impl Into<String>) -> Self {
        self.until = Some(until.into());
        self
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// Everything the proposal's reporting paragraph asks for, for one lane/window.
///
/// Counts are `i64` and never negative. A field that is zero means the thing
/// did not happen in this window; there is no "unknown" spelling, because
/// every field is a `COUNT` over rows that either exist or do not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRouteReport {
    // ── classification ────────────────────────────────────────────────────
    pub class_inconclusive: i64,
    pub class_causal_failure: i64,
    pub class_unknown: i64,

    // ── action ────────────────────────────────────────────────────────────
    pub action_rerun_run: i64,
    pub action_reenqueue: i64,
    pub action_ask_lead: i64,
    pub action_hold: i64,
    pub action_discard: i64,

    // ── provider ──────────────────────────────────────────────────────────
    /// Accepted `rerun_failed_jobs` episodes.
    pub retriggered: i64,
    /// Accepted `enable_auto_merge` episodes.
    pub reenqueued: i64,
    /// Explicit provider errors after `calling`. Charged and terminal.
    pub provider_action_failures: i64,
    /// `calling` rows whose external result is unknowable after a quiescent
    /// owner handoff.
    pub outcome_unknown: i64,

    // ── suppressions, reported separately by boundary ─────────────────────
    //
    // The proposal asks for these three "separately", and they are genuinely
    // different failures: the first costs nothing, the second wasted a lease,
    // the third wasted a whole Lead session.
    /// Obsolete before the provider call. Consumed no charge.
    pub suppressed_before_provider_call: i64,
    /// Obsolete at the Tier-2 lease boundary. No Lead session was dispatched.
    pub suppressed_before_lead_dispatch: i64,
    /// Obsolete when the supervisor tried to apply a delivered Lead result.
    /// A Lead session was spent and its result discarded.
    pub suppressed_before_supervisor_apply: i64,
    /// Obsolete after the provider call, discovered at startup recovery.
    pub superseded_after_call: i64,
    /// Evidence that deterministically held without a route decision.
    ///
    /// A **route row** whose single terminal outcome is `held`. Not to be
    /// confused with [`Self::recoverable_holds`], which counts a different
    /// population entirely — see that field.
    pub held: i64,

    // ── the bounded incomplete-evidence hold ──────────────────────────────
    //
    // These two read `ci_incomplete_hold_streaks`, NOT `ci_route_attempts`,
    // and that is the whole point of reporting them separately.
    /// Hold streaks currently holding: `poll_count > 0` and not yet escalated.
    ///
    /// **Disjoint from [`Self::held`], not a subset of it.** A recoverable hold
    /// writes *no route row at all* — that is what "hold and poll again" means
    /// — so it is invisible to every other count in this report. `held` counts
    /// routes that reached a terminal `held` outcome, which is a decision;
    /// this counts lanes that have not reached any decision yet. Two numbers
    /// that never overlap, and an operator asking why nothing is merging needs
    /// this one.
    pub recoverable_holds: i64,
    /// Hold streaks that reached the bound and escalated, each of which spent
    /// exactly one diagnose-only run-absent Tier-2 route.
    ///
    /// Counted from `escalated_at`, so it stays true after the escalation's
    /// route has terminalized.
    pub bounded_hold_escalations: i64,

    // ── recovery ──────────────────────────────────────────────────────────
    /// Pre-call resumptions summed across rows. N resumptions still charge once.
    pub resumed_pre_call_recoveries: i64,
    /// `calling` recoveries that took the row after a quiescent handoff.
    pub quiescent_owner_recoveries: i64,
    /// Recovery attempts that correctly left a live owner alone.
    pub live_owner_recovery_deferrals: i64,

    // ── budget ────────────────────────────────────────────────────────────
    /// Rows that reached `calling` and therefore hold a charge, monotonically.
    pub charged_slots: i64,
    /// Routes that reached Tier 2 through an exhausted budget.
    pub retry_exhausted: i64,

    // ── Tier 2 ────────────────────────────────────────────────────────────
    /// Leases opened. One per current-evidence adjudication.
    pub tier2_leases_opened: i64,
    /// Lead **sessions** actually dispatched and bound to a lease, summed over
    /// `lead_session_count`. Two sessions on one route count as two — which is
    /// the whole reason that column exists.
    pub lead_invocations: i64,
    /// Grounded remedies with a repository-valid command.
    pub repair_reopens: i64,
    /// Explicit unknown-boundary reopens, Lead's own and fallbacks together.
    pub diagnostic_reopens: i64,
    /// The subset of `diagnostic_reopens` that replaced a delivered result
    /// this contract refused (a wrong plan, an invented command, a park on a
    /// route that must reopen).
    pub diagnostic_reopens_from_rejected_result: i64,
    /// The subset of `diagnostic_reopens` where Lead produced **no** result at
    /// all — timeout, no finalize call, wrong finalize tool. Reported apart
    /// from the line above because the remedy is different: this one is a
    /// deadline or a prompt, that one is a contract violation.
    pub diagnostic_reopens_from_absent_result: i64,
    /// Parks that carry a **non-blank justification**, filtered on the
    /// justification text rather than on the `parked` label.
    ///
    /// Migration 195's `ci_route_attempts_park_cited_check` makes an uncited
    /// park unstorable, so in practice this equals the park count — and that
    /// equality is the assertion, not an excuse to stop measuring it. A filter
    /// on the label alone would report the same number with the CHECK dropped.
    pub parks_with_cited_cause: i64,
    pub supersedes: i64,

    // ── worker cost ───────────────────────────────────────────────────────
    /// Reopens that dispatch exactly one worker each: repairs plus diagnoses.
    pub worker_reopens: i64,
    /// Distinct PRs whose routes reached `merged`.
    pub merged_prs: i64,
    /// Routes closed by a newer passing observation.
    pub passed: i64,
}

impl CiRouteReport {
    /// Lead sessions spent per merged PR in this window.
    ///
    /// `None` when nothing merged: a ratio over zero merges is not "zero
    /// sessions per PR", it is an unanswerable question, and returning 0.0
    /// would read as the best possible result.
    #[must_use]
    pub fn lead_sessions_per_merged_pr(&self) -> Option<f64> {
        (self.merged_prs > 0).then(|| self.lead_invocations as f64 / self.merged_prs as f64)
    }

    /// Worker reopens spent per merged PR in this window. `None` when nothing
    /// merged, for the same reason.
    #[must_use]
    pub fn worker_reopens_per_merged_pr(&self) -> Option<f64> {
        (self.merged_prs > 0).then(|| self.worker_reopens as f64 / self.merged_prs as f64)
    }

    /// Every obsolete route this window suppressed, at any of the three
    /// boundaries. Reported alongside the split, never instead of it.
    #[must_use]
    pub fn total_obsolete_suppressions(&self) -> i64 {
        self.suppressed_before_provider_call
            + self.suppressed_before_lead_dispatch
            + self.suppressed_before_supervisor_apply
            + self.superseded_after_call
    }
}

// ---------------------------------------------------------------------------
// Rollback quiescence report
// ---------------------------------------------------------------------------

/// The two counts no SQL query can answer, attested by the coordinator that
/// records the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRouteQuiescenceAttestation {
    /// Registered provider-action futures in the recorder's in-process scope.
    pub registered_provider_futures: i64,
    /// Route identities that remain the current failed evidence for their PR
    /// or merge-group lane — the evidence-advance high-watermark. Zero means
    /// every routed identity advanced, passed, or merged.
    pub current_failed_identities: i64,
}

/// One durable rollback quiescence report.
///
/// The proposal permits a binary rollback "only after **one**
/// repository-checkable report records zero" across six counts. One report,
/// not six queries: an operator must be able to point at a single row taken at
/// a single instant, rather than at six answers that were each true at a
/// different moment while a route advanced between them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRollbackQuiescenceReport {
    pub id: String,
    /// The gate posture when the report was taken. A report taken while the
    /// gate is still `enabled` can never permit a rollback: new routes are
    /// still being admitted, so every count is a snapshot of a moving target.
    pub gate_state: String,
    pub reserved_rows: i64,
    pub calling_rows: i64,
    pub open_tier2_leases: i64,
    pub unapplied_lead_results: i64,
    pub registered_provider_futures: i64,
    pub current_failed_identities: i64,
    pub permits_rollback: bool,
    pub recorded_by_incarnation: String,
    pub recorded_at: String,
}

impl CiRollbackQuiescenceReport {
    /// The verdict, recomputed from the stored counts.
    ///
    /// Deliberately **not** a read of the stored `permits_rollback` column:
    /// this is the function the column is checked against, so a caller that
    /// consults it can never be told "yes" by a row whose boolean was written
    /// wrong. Migration 193 enforces the same equality with a CHECK, which
    /// makes the two independent witnesses of one fact.
    #[must_use]
    pub fn recomputed_verdict(&self) -> bool {
        self.gate_state != "enabled"
            && self.reserved_rows == 0
            && self.calling_rows == 0
            && self.open_tier2_leases == 0
            && self.unapplied_lead_results == 0
            && self.registered_provider_futures == 0
            && self.current_failed_identities == 0
    }

    /// Every count that is not yet zero, in the proposal's order. Empty
    /// exactly when the report permits a rollback.
    #[must_use]
    pub fn blocking_reasons(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.gate_state == "enabled" {
            out.push("ci_evidence_routing is still enabled: disable it first".to_owned());
        }
        for (count, label) in [
            (self.reserved_rows, "reserved rows"),
            (self.calling_rows, "calling rows"),
            (self.open_tier2_leases, "active Tier-2 leases"),
            (self.unapplied_lead_results, "unapplied Lead results"),
            (
                self.registered_provider_futures,
                "registered provider-action futures",
            ),
            (
                self.current_failed_identities,
                "route identities that are still the current failed evidence for their lane",
            ),
        ] {
            if count != 0 {
                out.push(format!("{count} {label}"));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

impl CiRouteAttemptRepository {
    /// Build the routing report for one lane/time window.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn route_report(&self, filter: &CiRouteReportFilter) -> Result<CiRouteReport> {
        self.db().ensure_initialized().await?;

        // `$1/$2/$3` are all nullable and each condition is `x IS NULL OR ...`,
        // so one prepared statement serves every combination of filters. The
        // alternative — string-concatenating the WHERE clause — is how a lane
        // filter ends up silently unapplied on one of the eight branches.
        let lane = filter.lane.map(CiLane::as_str);
        let row = sqlx::query(
            "WITH scoped AS (
               SELECT *, COALESCE(tier2_resolution, terminal_outcome) AS adjudicated
               FROM ci_route_attempts
               WHERE ($1::text IS NULL OR lane = $1)
                 AND ($2::text IS NULL OR reserved_at >= $2::timestamptz)
                 AND ($3::text IS NULL OR reserved_at <  $3::timestamptz)
             )
             SELECT
               COUNT(*) FILTER (WHERE class = 'inconclusive')    AS class_inconclusive,
               COUNT(*) FILTER (WHERE class = 'causal_failure')  AS class_causal_failure,
               COUNT(*) FILTER (WHERE class = 'unknown')         AS class_unknown,

               COUNT(*) FILTER (WHERE action = 'rerun_run')      AS action_rerun_run,
               COUNT(*) FILTER (WHERE action = 'reenqueue')      AS action_reenqueue,
               COUNT(*) FILTER (WHERE action = 'ask_lead')       AS action_ask_lead,
               COUNT(*) FILTER (WHERE action = 'hold')           AS action_hold,
               COUNT(*) FILTER (WHERE action = 'discard')        AS action_discard,

               COUNT(*) FILTER (WHERE terminal_outcome = 'retriggered')   AS retriggered,
               COUNT(*) FILTER (WHERE terminal_outcome = 'reenqueued')    AS reenqueued,
               COUNT(*) FILTER (WHERE terminal_outcome = 'action_failed') AS provider_action_failures,
               COUNT(*) FILTER (WHERE terminal_outcome = 'outcome_unknown') AS outcome_unknown,

               COUNT(*) FILTER (WHERE terminal_outcome = 'superseded_pre_call')       AS suppressed_before_provider_call,
               COUNT(*) FILTER (WHERE terminal_outcome = 'superseded_before_lead')    AS suppressed_before_lead_dispatch,
               COUNT(*) FILTER (WHERE adjudicated       = 'superseded_before_apply')  AS suppressed_before_supervisor_apply,
               COUNT(*) FILTER (WHERE terminal_outcome = 'superseded_after_call')     AS superseded_after_call,
               COUNT(*) FILTER (WHERE terminal_outcome = 'held')                      AS held,

               COALESCE(SUM(pre_call_resumptions), 0)::bigint             AS resumed_pre_call_recoveries,
               COUNT(*) FILTER (WHERE charged_signature_count IS NOT NULL) AS charged_slots,
               COUNT(*) FILTER (WHERE retry_exhausted_at IS NOT NULL)      AS retry_exhausted,

               COUNT(*) FILTER (WHERE tier2_lease_state IS NOT NULL)  AS tier2_leases_opened,
               -- Lead SESSIONS, not routes-that-had-a-session. `lead_session_id`
               -- is one slot holding the FIRST session, so counting non-NULL
               -- ids caps this metric at one per route -- and Lead sessions per
               -- merged PR is the exact cost the proposal bounds, so the cap
               -- sat on the number being watched.
               COALESCE(SUM(lead_session_count), 0)::bigint          AS lead_invocations,

               COUNT(*) FILTER (WHERE adjudicated = 'repair_reopened')     AS repair_reopens,
               COUNT(*) FILTER (WHERE adjudicated = 'diagnostic_reopened') AS diagnostic_reopens,
               COUNT(*) FILTER (WHERE adjudicated = 'diagnostic_reopened'
                                  AND lead_rejection IS NOT NULL
                                  AND lead_rejection NOT IN ('timed_out', 'no_result', 'unsupported_finalize_tool'))
                                                                          AS diagnostic_reopens_rejected,
               COUNT(*) FILTER (WHERE adjudicated = 'diagnostic_reopened'
                                  AND lead_rejection IN ('timed_out', 'no_result', 'unsupported_finalize_tool'))
                                                                          AS diagnostic_reopens_absent,
               -- The CITED cause, not the label. Filtering on `adjudicated =
               -- 'parked'` alone would have made this metric a restatement of
               -- the outcome column: it would report the same number whether or
               -- not any park carried a justification. Migration 195's
               -- `ci_route_attempts_park_cited_check` makes the two agree, which
               -- is the point — the metric and the constraint are now
               -- independent witnesses of one fact instead of one fact quoted
               -- twice.
               COUNT(*) FILTER (WHERE adjudicated = 'parked'
                                  AND park_justification IS NOT NULL
                                  AND btrim(park_justification) <> '') AS parks_with_cited_cause,
               COUNT(*) FILTER (WHERE adjudicated = 'superseded') AS supersedes,
               COUNT(*) FILTER (WHERE adjudicated IN ('repair_reopened', 'diagnostic_reopened'))
                                                                  AS worker_reopens,
               -- The DENOMINATOR of both per-merged-PR ratios, so an
               -- undercount here inflates every cost number in the report.
               -- `terminal_outcome = 'merged'` misses a merge that landed in
               -- `tier2_resolution` — which is what happens whenever the route
               -- had already terminalized on its provider result, i.e. exactly
               -- the population that reached Lead. Same `adjudicated` reading
               -- as every other outcome count here.
               COUNT(DISTINCT pr_number) FILTER (WHERE adjudicated = 'merged') AS merged_prs,
               COUNT(*) FILTER (WHERE terminal_outcome = 'passed') AS passed
             FROM scoped",
        )
        .bind(lane)
        .bind(filter.since.as_deref())
        .bind(filter.until.as_deref())
        .fetch_one(self.db().pool())
        .await?;

        // Recovery counts live on a different table and are scoped by the same
        // window through the attempt they belong to.
        let recoveries = sqlx::query(
            "SELECT
               COUNT(*) FILTER (WHERE r.cas_won) AS quiescent_owner_recoveries,
               COUNT(*) FILTER (WHERE r.recovery_reason = $4) AS live_owner_recovery_deferrals
             FROM ci_route_calling_recoveries r
             JOIN ci_route_attempts a
               ON a.subject_kind = r.subject_kind
              AND a.subject_id = r.subject_id
              AND a.provider_action_key = r.provider_action_key
             WHERE ($1::text IS NULL OR a.lane = $1)
               AND ($2::text IS NULL OR a.reserved_at >= $2::timestamptz)
               AND ($3::text IS NULL OR a.reserved_at <  $3::timestamptz)",
        )
        .bind(lane)
        .bind(filter.since.as_deref())
        .bind(filter.until.as_deref())
        .bind(CiCallingRecoveryReason::LiveOwnerDeferred.as_str())
        .fetch_one(self.db().pool())
        .await?;

        // The bounded hold lives on its own table and is scoped by the streak's
        // OWN lane and `created_at`.
        //
        // `created_at` rather than `updated_at`, deliberately: a hold that has
        // been polling for hours has a fresh `updated_at`, so an
        // `updated_at`-keyed window would report it in every window except the
        // one it started in — and a long-running hold is exactly the population
        // an operator asking "why is nothing merging" is looking for. Keying on
        // creation matches `reserved_at` above, which is the same choice for
        // the same reason.
        let holds = sqlx::query(
            "SELECT
               COUNT(*) FILTER (WHERE poll_count > 0 AND escalated_at IS NULL) AS recoverable_holds,
               COUNT(*) FILTER (WHERE escalated_at IS NOT NULL) AS bounded_hold_escalations
             FROM ci_incomplete_hold_streaks
             WHERE ($1::text IS NULL OR lane = $1)
               AND ($2::text IS NULL OR created_at >= $2::timestamptz)
               AND ($3::text IS NULL OR created_at <  $3::timestamptz)",
        )
        .bind(lane)
        .bind(filter.since.as_deref())
        .bind(filter.until.as_deref())
        .fetch_one(self.db().pool())
        .await?;

        Ok(CiRouteReport {
            class_inconclusive: row.try_get("class_inconclusive")?,
            class_causal_failure: row.try_get("class_causal_failure")?,
            class_unknown: row.try_get("class_unknown")?,
            action_rerun_run: row.try_get("action_rerun_run")?,
            action_reenqueue: row.try_get("action_reenqueue")?,
            action_ask_lead: row.try_get("action_ask_lead")?,
            action_hold: row.try_get("action_hold")?,
            action_discard: row.try_get("action_discard")?,
            retriggered: row.try_get("retriggered")?,
            reenqueued: row.try_get("reenqueued")?,
            provider_action_failures: row.try_get("provider_action_failures")?,
            outcome_unknown: row.try_get("outcome_unknown")?,
            suppressed_before_provider_call: row.try_get("suppressed_before_provider_call")?,
            suppressed_before_lead_dispatch: row.try_get("suppressed_before_lead_dispatch")?,
            suppressed_before_supervisor_apply: row
                .try_get("suppressed_before_supervisor_apply")?,
            superseded_after_call: row.try_get("superseded_after_call")?,
            held: row.try_get("held")?,
            recoverable_holds: holds.try_get("recoverable_holds")?,
            bounded_hold_escalations: holds.try_get("bounded_hold_escalations")?,
            resumed_pre_call_recoveries: row.try_get("resumed_pre_call_recoveries")?,
            quiescent_owner_recoveries: recoveries.try_get("quiescent_owner_recoveries")?,
            live_owner_recovery_deferrals: recoveries.try_get("live_owner_recovery_deferrals")?,
            charged_slots: row.try_get("charged_slots")?,
            retry_exhausted: row.try_get("retry_exhausted")?,
            tier2_leases_opened: row.try_get("tier2_leases_opened")?,
            lead_invocations: row.try_get("lead_invocations")?,
            repair_reopens: row.try_get("repair_reopens")?,
            diagnostic_reopens: row.try_get("diagnostic_reopens")?,
            diagnostic_reopens_from_rejected_result: row.try_get("diagnostic_reopens_rejected")?,
            diagnostic_reopens_from_absent_result: row.try_get("diagnostic_reopens_absent")?,
            parks_with_cited_cause: row.try_get("parks_with_cited_cause")?,
            supersedes: row.try_get("supersedes")?,
            worker_reopens: row.try_get("worker_reopens")?,
            merged_prs: row.try_get("merged_prs")?,
            passed: row.try_get("passed")?,
        })
    }

    /// Count route identities that remain the **current failed evidence** for
    /// their PR-or-merge-group lane — the evidence-advance high-watermark.
    ///
    /// A routed identity has advanced when the same `(lane, pr_number)` has a
    /// strictly newer route row on a different `(pr_head_sha, run_id)`, or when
    /// this row itself reached `passed` or `merged`. Anything else is evidence
    /// an old binary — which ignores these rows entirely — would handle again
    /// from scratch, which is the exact failure the proposal's rollback gate
    /// exists to prevent.
    ///
    /// # Run-absent identities
    ///
    /// `run_id` is nullable since migration 195. The advance test is written as
    /// a **row-constructor** `IS DISTINCT FROM`, which is NULL-safe in both
    /// directions: `(sha, NULL) IS DISTINCT FROM (sha, NULL)` is `false` (the
    /// same identity, so no advance) and `(sha, 7) IS DISTINCT FROM (sha, NULL)`
    /// is `true` (a run-absent capture followed by a real run *is* an advance).
    /// A plain `newer.run_id <> a.run_id` would evaluate to NULL and be dropped
    /// by the `WHERE`, so a run-absent row would never be seen to advance and
    /// the rollback gate would stay red forever. This is asserted by test, not
    /// by this paragraph.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn current_failed_identity_count(&self) -> Result<i64> {
        self.db().ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT COUNT(*) AS n
             FROM ci_route_attempts a
             WHERE COALESCE(a.tier2_resolution, a.terminal_outcome) IS DISTINCT FROM 'passed'
               AND COALESCE(a.tier2_resolution, a.terminal_outcome) IS DISTINCT FROM 'merged'
               AND NOT EXISTS (
                 SELECT 1 FROM ci_route_attempts newer
                 WHERE newer.subject_kind = a.subject_kind
                   AND newer.subject_id = a.subject_id
                   AND newer.lane = a.lane
                   AND newer.pr_number = a.pr_number
                   AND (newer.pr_head_sha, newer.run_id) IS DISTINCT FROM (a.pr_head_sha, a.run_id)
                   AND newer.reserved_at > a.reserved_at
               )",
        )
        .fetch_one(self.db().pool())
        .await?;
        Ok(row.try_get("n")?)
    }

    /// Take, verify, and durably record one rollback quiescence report.
    ///
    /// The six counts are read inside **one** transaction so the report is a
    /// single instant rather than six. `attestation` supplies the two counts no
    /// query can answer; passing a stale or invented value is the one way to
    /// get a wrong verdict out of this function, which is why both are stored
    /// on the row rather than merely consulted.
    ///
    /// Returns the recorded report. Callers read
    /// [`CiRollbackQuiescenceReport::permits_rollback`]; a `false` is not an
    /// error and the report is recorded either way, because a blocked rollback
    /// attempt is exactly the thing an operator needs to see afterwards.
    ///
    /// # Errors
    ///
    /// Database failures, or a negative attested count.
    pub async fn record_rollback_quiescence_report(
        &self,
        gate_state: &str,
        recorded_by_incarnation: &str,
        attestation: CiRouteQuiescenceAttestation,
    ) -> Result<CiRollbackQuiescenceReport> {
        self.db().ensure_initialized().await?;
        if attestation.registered_provider_futures < 0 || attestation.current_failed_identities < 0
        {
            return Err(DbError::InvalidData(
                "attested quiescence counts cannot be negative".to_owned(),
            ));
        }
        if !matches!(gate_state, "enabled" | "quiescing" | "disabled_clean") {
            return Err(DbError::InvalidData(format!(
                "unknown ci_evidence_routing gate state `{gate_state}`"
            )));
        }

        let mut tx = self.db().pool().begin().await?;
        let counts = sqlx::query(
            "SELECT \
               COUNT(*) FILTER (WHERE action_phase = 'reserved') AS reserved_rows, \
               COUNT(*) FILTER (WHERE action_phase = 'calling') AS calling_rows, \
               COUNT(*) FILTER (WHERE tier2_lease_state = 'open') AS open_tier2_leases, \
               COUNT(*) FILTER (WHERE tier2_lease_state = 'open' AND lead_session_id IS NOT NULL) AS unapplied_lead_results \
             FROM ci_route_attempts",
        )
        .fetch_one(&mut *tx)
        .await?;
        let reserved_rows: i64 = counts.try_get("reserved_rows")?;
        let calling_rows: i64 = counts.try_get("calling_rows")?;
        let open_tier2_leases: i64 = counts.try_get("open_tier2_leases")?;
        let unapplied_lead_results: i64 = counts.try_get("unapplied_lead_results")?;

        let permits_rollback = gate_state != "enabled"
            && reserved_rows == 0
            && calling_rows == 0
            && open_tier2_leases == 0
            && unapplied_lead_results == 0
            && attestation.registered_provider_futures == 0
            && attestation.current_failed_identities == 0;

        let id = uuid::Uuid::now_v7().to_string();
        let row = sqlx::query(
            "INSERT INTO ci_route_rollback_reports (
                 id, gate_state, reserved_rows, calling_rows, open_tier2_leases,
                 unapplied_lead_results, registered_provider_futures,
                 current_failed_identities, permits_rollback, recorded_by_incarnation
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING to_char(recorded_at AT TIME ZONE 'UTC', \
                               'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS recorded_at",
        )
        .bind(&id)
        .bind(gate_state)
        .bind(reserved_rows)
        .bind(calling_rows)
        .bind(open_tier2_leases)
        .bind(unapplied_lead_results)
        .bind(attestation.registered_provider_futures)
        .bind(attestation.current_failed_identities)
        .bind(permits_rollback)
        .bind(recorded_by_incarnation)
        .fetch_one(&mut *tx)
        .await?;
        let recorded_at: String = row.try_get("recorded_at")?;
        tx.commit().await?;

        Ok(CiRollbackQuiescenceReport {
            id,
            gate_state: gate_state.to_owned(),
            reserved_rows,
            calling_rows,
            open_tier2_leases,
            unapplied_lead_results,
            registered_provider_futures: attestation.registered_provider_futures,
            current_failed_identities: attestation.current_failed_identities,
            permits_rollback,
            recorded_by_incarnation: recorded_by_incarnation.to_owned(),
            recorded_at,
        })
    }

    /// The most recent rollback quiescence report, if one was ever taken.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn latest_rollback_quiescence_report(
        &self,
    ) -> Result<Option<CiRollbackQuiescenceReport>> {
        self.db().ensure_initialized().await?;
        let row = sqlx::query(
            "SELECT id, gate_state, reserved_rows, calling_rows, open_tier2_leases, \
                    unapplied_lead_results, registered_provider_futures, \
                    current_failed_identities, permits_rollback, recorded_by_incarnation, \
                    to_char(recorded_at AT TIME ZONE 'UTC', \
                            'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS recorded_at \
             FROM ci_route_rollback_reports ORDER BY recorded_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(self.db().pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(CiRollbackQuiescenceReport {
            id: row.try_get("id")?,
            gate_state: row.try_get("gate_state")?,
            reserved_rows: row.try_get("reserved_rows")?,
            calling_rows: row.try_get("calling_rows")?,
            open_tier2_leases: row.try_get("open_tier2_leases")?,
            unapplied_lead_results: row.try_get("unapplied_lead_results")?,
            registered_provider_futures: row.try_get("registered_provider_futures")?,
            current_failed_identities: row.try_get("current_failed_identities")?,
            permits_rollback: row.try_get("permits_rollback")?,
            recorded_by_incarnation: row.try_get("recorded_by_incarnation")?,
            recorded_at: row.try_get("recorded_at")?,
        }))
    }
}
