//! 4etb: the adjudication child-close transaction.
//!
//! Two things must happen inside the SAME transaction that closes an
//! adjudication child (a `planner-park-escalation` or a legacy
//! `human-review-hold`):
//!
//! 1. **Exhausted-ladder ownership.** `PlannerEscalation` is the terminal
//!    adjudication rung. Rounds 1 and 2 may close unchanged and release the
//!    source into at most the next idempotent round. When ROUND 3 closes
//!    without changing the source disposition, the ladder is spent and the
//!    source must not be left in a status nobody polls — the exact failure of
//!    tasks `z8i8` and `zkas`, which exhausted the old ladder with unmerged PRs
//!    and then sat `open`, unscanned, until a human moved them. Exactly one
//!    branch is taken: a source with an open, unmerged PR goes to `pr_review`
//!    (owner: the PR poller); a source with no PR is force-closed with the
//!    contractual reason; an already-terminal or already-`pr_review` source is
//!    preserved. A fourth `PlannerEscalation` is forbidden.
//!
//! 2. **The adjudication outcome event.** Every adjudication child close emits
//!    `adjudication_outcome` with `source_changed` / `source_unchanged` plus
//!    `source_status_before` / `source_status_after`. The value describes ONLY
//!    that transaction — it is not a claim that the task eventually merged or
//!    converged, and no longitudinal reporting is derived from it.
//!
//! Doing both here rather than in the coordinator is what makes the
//! intermediate merely-`open` source unobservable: the terminal child close and
//! the source transition commit together.

use sqlx::Row;

use crate::Result;

/// Activity `event_type` for the per-adjudication disposition outcome.
pub const ADJUDICATION_OUTCOME_EVENT: &str = "adjudication_outcome";

/// `adjudication_outcome` value: the child close changed the source's business
/// disposition.
pub const SOURCE_CHANGED: &str = "source_changed";

/// `adjudication_outcome` value: the child close left the source's business
/// disposition untouched. Coordinator-owned scaffolding (closing the child,
/// consuming an arbitration row, writing an evidence epoch, writing a
/// hold-ceiling or park-redispatch marker, releasing a blocker, replacing one
/// escalation blocker with the next) does not by itself make the source
/// changed.
pub const SOURCE_UNCHANGED: &str = "source_unchanged";

/// Terminal close reason for a no-PR source whose adjudication ladder is spent.
/// Exact text is contractual — operators and tests match on it.
///
/// It is the transition REASON, not the `close_reason` column. The column
/// carries a fixed vocabulary everywhere else (`completed`, `force_closed`,
/// `parent_closed`, `superseded`, …), and the coordinator-side copy of this
/// contract routes through `terminally_fail_task`, which writes
/// [`TERMINAL_CLOSE_REASON_CLASS`] there and puts the sentence on the
/// `status_changed` activity. Writing the free-text sentence into the column
/// here would have meant no single operator query finds both copies.
pub const LADDER_EXHAUSTED_CLOSE_REASON: &str =
    "adjudication ladder exhausted without an actionable PR";

/// `close_reason` column value for the exhausted-ladder terminal close — the
/// same class `terminally_fail_task` writes, so both copies of the contract
/// agree.
pub const TERMINAL_CLOSE_REASON_CLASS: &str = "force_closed";

/// Terminal-rung ceiling: `PlannerEscalation` rounds 1, 2 and 3 are permitted;
/// the unchanged close of round 3 invokes exhausted ownership and round 4 is
/// never created.
///
/// Round number is the durable blocker-derived count of held-remediation
/// children on the source, never a task counter column.
pub const MAX_AUTONOMOUS_ESCALATIONS: i64 = 3;

/// The fixed business-disposition snapshot the outcome compares.
///
/// Deliberately narrow: status, terminal close reason, the scope fields, the
/// applied reopen/rescope directive, and the supersession/replacement
/// relationship. Nothing else on the row participates.
#[derive(Clone, Debug)]
pub struct BusinessDisposition {
    pub status: String,
    pub close_reason: Option<String>,
    pub description: String,
    pub design: String,
    pub acceptance_criteria: String,
    /// Applied reopen/rescope directive — the arbiter directive the source is
    /// currently carrying, `None` when no directive has been applied.
    pub applied_directive: Option<String>,
}

// NOTE on the supersession/replacement relationship: tasks carry no
// `superseded_by` column, and none of the four fields above is one.
//
// What a supersede actually writes is worth stating precisely, because the
// enum names are misleading: `TransitionAction::ArbiterSupersede` writes status
// `closed` and `close_reason = CLOSE_REASON_FORCE_CLOSED` ("force_closed") —
// NOT `TaskStatus::Superseded` / `CLOSE_REASON_SUPERSEDED`, which exist but
// which no arbiter path writes. (An operator grepping for
// `status = 'superseded'` to find superseded sources will find nothing.)
//
// Either way the source leaves the parked state and acquires a terminal close
// reason, and BOTH of those are in this snapshot — so a supersede always reads
// as `source_changed` without inventing a relationship column the schema does
// not have. `adjudication_close_tests::a_supersede_before_the_close_reads_source_changed`
// asserts that against the schema and against both representations.

/// Activity `event_type` carrying the source's business disposition as it stood
/// when an adjudication child was CREATED.
///
/// The outcome compares the source before and after the adjudication, and the
/// planner's own edits (rescope, applied directive, supersede) land through
/// separate MCP calls BEFORE it closes the child. Reading `before` off the live
/// row inside the close transaction would therefore always see those edits
/// already applied and report `source_unchanged` for every one of them — and,
/// worse, would let the exhausted-ladder branch force-close a source the
/// planner had just legitimately rescoped. The snapshot written at child
/// creation is the only honest `before`.
pub const ADJUDICATION_SOURCE_SNAPSHOT_EVENT: &str = "adjudication_source_snapshot";

/// Statuses the coordinator can park a source to `open` FROM when it holds it
/// behind an adjudication child.
///
/// This is exactly `TransitionAction::ParkForRemediation`'s legal from-set
/// (`djinn_core::models::task`), which is what both producers ultimately apply
/// — `escalate_to_planner_or_terminally_fail` via `park_source_open`, and the
/// arbiter via `ArbiterPark`. Keeping the two in step is the point: any state
/// the park is legal from is a state the snapshot can be taken in.
pub const PARKABLE_FROM_STATUSES: &[&str] = &[
    "pr_draft",
    "pr_review",
    "in_progress",
    "open",
    "needs_task_review",
    "in_task_review",
    "needs_lead_intervention",
    "in_lead_intervention",
    "approved",
];

/// Is the status delta `before -> after` nothing but the coordinator parking the
/// source behind its adjudication child?
///
/// Both producers record the creation-time snapshot BEFORE the park transition
/// (`escalate_to_planner_or_terminally_fail` calls `create_remediation_task`
/// then `park_source_open`; the agent's `execute_arbiter_park_transaction`
/// snapshots before `ArbiterPark`). So `before` is whatever in-flight state the
/// trigger caught the source in and `after` is `open`. That is the coordinator
/// holding the source while somebody else decides — not a disposition. The
/// spec is explicit that this scaffolding "does not by itself make the source
/// changed".
///
/// This mattered enormously in production and not at all in the fixture. With
/// no exclusion, EVERY close read `source_changed`, truth-table row 1 was
/// violated, and the exhausted-ladder branch — which only fires on an UNCHANGED
/// close — never ran. A round-3 close therefore left the source `open` with an
/// unmerged PR and no owner: precisely the `z8i8`/`zkas` stranding this
/// proposal exists to end.
///
/// **Directional on purpose.** The rule fires only when `after` is `open`, so
/// the reverse deltas that ARE dispositions still read as changes — most
/// importantly `open -> pr_review`, which is the exhausted-ladder handoff
/// itself. A symmetric "treat these statuses as equal" rule would have made
/// that handoff invisible.
///
/// An earlier version collapsed only the two Lead-intervention statuses, which
/// left the defect alive for every producer that snapshots from a PR status:
/// the merge-method wedge (`poll_pr_review_tasks` escalates straight off a
/// `pr_review` row) and the human-adjudicated tripwire hold (reached from
/// `pr_draft`) — the same z8i8/zkas family. Deriving the set from the park's
/// own legal from-set closes it for every producer at once.
fn is_scaffolding_park(before: &str, after: &str) -> bool {
    after == "open" && before != "open" && PARKABLE_FROM_STATUSES.contains(&before)
}

impl BusinessDisposition {
    /// Is this the same business disposition as `other`, ignoring the
    /// coordinator's adjudication scaffolding? See [`parked_equivalent`].
    fn same_disposition_as(&self, other: &Self) -> bool {
        // `self` is the CLOSE-time reading, `other` the creation-time snapshot,
        // so the delta under test is `other.status -> self.status`.
        let status_unchanged =
            self.status == other.status || is_scaffolding_park(&other.status, &self.status);
        status_unchanged
            && self.close_reason == other.close_reason
            && self.description == other.description
            && self.design == other.design
            && self.acceptance_criteria == other.acceptance_criteria
            && self.applied_directive == other.applied_directive
    }

    fn to_payload(&self, child_id: &str) -> serde_json::Value {
        serde_json::json!({
            "adjudication_child_id": child_id,
            "status": self.status,
            "close_reason": self.close_reason,
            "description": self.description,
            "design": self.design,
            "acceptance_criteria": self.acceptance_criteria,
            "applied_directive": self.applied_directive,
        })
    }

    fn from_payload(payload: &serde_json::Value) -> Option<Self> {
        Some(Self {
            status: payload["status"].as_str()?.to_owned(),
            close_reason: payload["close_reason"].as_str().map(str::to_owned),
            description: payload["description"].as_str()?.to_owned(),
            design: payload["design"].as_str()?.to_owned(),
            acceptance_criteria: payload["acceptance_criteria"].as_str()?.to_owned(),
            applied_directive: payload["applied_directive"].as_str().map(str::to_owned),
        })
    }

    /// Read the snapshot inside a transaction.
    pub async fn read_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        task_id: &str,
    ) -> Result<Option<Self>> {
        let row = sqlx::query(
            r#"SELECT
                    t.status,
                    t.close_reason,
                    t.description,
                    t.design,
                    t.acceptance_criteria::text AS acceptance_criteria,
                    (SELECT a.directive::text
                       FROM task_arbitrations a
                      WHERE a.task_id = t.id AND a.directive_injected = TRUE
                      ORDER BY a.hold_cycle DESC LIMIT 1) AS applied_directive
                 FROM tasks t WHERE t.id = $1"#,
        )
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.map(|row| Self {
            status: row.get("status"),
            close_reason: row.get("close_reason"),
            description: row.get("description"),
            design: row.get("design"),
            acceptance_criteria: row.get("acceptance_criteria"),
            applied_directive: row.get("applied_directive"),
        }))
    }
}

/// Record the source's business disposition at the moment an adjudication child
/// is created, so the child's close can tell a REAL disposition change from the
/// coordinator's own scaffolding.
///
/// Idempotent per (source, child): a re-run keeps the FIRST snapshot, which is
/// the one that predates every planner edit.
pub async fn record_adjudication_source_snapshot(
    pool: &sqlx::PgPool,
    source_id: &str,
    child_id: &str,
) -> Result<()> {
    // Retry: a child whose snapshot is missing takes NO ownership branch when
    // it closes (see `apply_adjudication_child_close_tx`), which is a silent
    // return of the z8i8/zkas stranding. A transient write failure must not be
    // the thing that costs a source its owner, so a lost race with concurrent
    // writers is retried before the caller is told.
    let mut last_error = None;
    for _ in 0..3 {
        match record_adjudication_source_snapshot_once(pool, source_id, child_id).await {
            Ok(()) => return Ok(()),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.expect("loop runs at least once"))
}

async fn record_adjudication_source_snapshot_once(
    pool: &sqlx::PgPool,
    source_id: &str,
    child_id: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let existing = read_source_snapshot(&mut tx, source_id, child_id).await?;
    if existing.is_some() {
        tx.commit().await?;
        return Ok(());
    }
    let Some(snapshot) = BusinessDisposition::read_tx(&mut tx, source_id).await? else {
        tx.commit().await?;
        return Ok(());
    };
    let activity_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(&activity_id)
    .bind(source_id)
    .bind("coordinator")
    .bind("system")
    .bind(ADJUDICATION_SOURCE_SNAPSHOT_EVENT)
    .bind(snapshot.to_payload(child_id).to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn read_source_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_id: &str,
    child_id: &str,
) -> Result<Option<BusinessDisposition>> {
    let payload: Option<String> = sqlx::query_scalar(
        r#"SELECT payload::text FROM activity_log
            WHERE task_id = $1
              AND event_type = $2
              AND payload ->> 'adjudication_child_id' = $3
            ORDER BY created_at ASC
            LIMIT 1"#,
    )
    .bind(source_id)
    .bind(ADJUDICATION_SOURCE_SNAPSHOT_EVENT)
    .bind(child_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(payload
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| BusinessDisposition::from_payload(&value)))
}

/// Is `labels_json` an adjudication child — a task whose close releases the
/// source it blocks?
///
/// Mirrors the coordinator's `roles::releases_source_on_close`: the autonomous
/// `planner-park-escalation` and the legacy `human-review-hold`.
pub fn is_adjudication_child(labels_json: &str) -> bool {
    labels_json.contains("planner-park-escalation") || labels_json.contains("human-review-hold")
}

/// Does `task_id` have an open, unmerged PR?
///
/// An adopted PR counts — the task carries the adopted PR's URL exactly like
/// one it opened itself. A MERGED PR does not: `merge_commit_sha` is the
/// durable proof the PR landed, and a landed PR gives the poller nothing to do.
/// A source whose PR was closed on GitHub without merging has no durable
/// column to read; the PR poller is the named owner of that reconciliation, so
/// routing it to `pr_review` is still the correct handoff.
async fn has_open_unmerged_pr(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_id: &str,
) -> Result<Option<String>> {
    let row = sqlx::query("SELECT pr_url, merge_commit_sha FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let pr_url: Option<String> = row.get("pr_url");
    let merge_commit_sha: Option<String> = row.get("merge_commit_sha");
    if merge_commit_sha.is_some_and(|sha| !sha.trim().is_empty()) {
        return Ok(None);
    }
    Ok(pr_url.filter(|url| !url.trim().is_empty()))
}

/// Blocker-derived count of held-remediation (adjudication) children on
/// `source_id`, INCLUDING closed ones.
///
/// Each escalation adds exactly one blocker that is never removed, so this is
/// the terminal-rung ROUND NUMBER. No counter column and no migration.
pub async fn planner_escalation_count_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_id: &str,
) -> Result<i64> {
    // `blockers.task_id` is the BLOCKED source; `blockers.blocking_task_id` is
    // the blocker itself — the same direction `TaskRepository::list_blockers`
    // reads. Getting this backwards would count the source's DEPENDENTS and
    // silently never reach the ceiling.
    let count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)::bigint
             FROM blockers b
             JOIN tasks t ON t.id = b.blocking_task_id
            WHERE b.task_id = $1
              AND (t.labels::text LIKE '%planner-park-escalation%'
                OR t.labels::text LIKE '%human-review-hold%')"#,
    )
    .bind(source_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(count)
}

/// Has an `adjudication_outcome` event already been written for this
/// (source, child) pair? Duplicate delivery must emit no second event.
async fn outcome_already_emitted(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_id: &str,
    child_id: &str,
) -> Result<bool> {
    let exists: Option<i64> = sqlx::query_scalar(
        r#"SELECT 1::bigint FROM activity_log
            WHERE task_id = $1
              AND event_type = $2
              AND payload ->> 'adjudication_child_id' = $3
            LIMIT 1"#,
    )
    .bind(source_id)
    .bind(ADJUDICATION_OUTCOME_EVENT)
    .bind(child_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(exists.is_some())
}

/// Statuses that already have an owner and must be preserved verbatim by the
/// exhausted-ladder branch.
fn already_owned(status: &str) -> bool {
    matches!(status, "closed" | "superseded" | "pr_review")
}

/// Apply the 4etb adjudication child-close transaction for one closing child.
///
/// Called from `TaskRepository::transition` with the child's own transaction,
/// AFTER the child row has been closed and BEFORE the commit. Returns the
/// number of source tasks for which an outcome event was emitted.
pub async fn apply_adjudication_child_close_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    child_id: &str,
    child_labels: &str,
) -> Result<usize> {
    if !is_adjudication_child(child_labels) {
        return Ok(0);
    }

    let source_ids: Vec<String> =
        sqlx::query_scalar("SELECT task_id FROM blockers WHERE blocking_task_id = $1")
            .bind(child_id)
            .fetch_all(&mut **tx)
            .await?;

    let mut emitted = 0usize;
    for source_id in source_ids {
        // Duplicate closure delivery: the transaction is an idempotent no-op
        // and emits no second event.
        if outcome_already_emitted(tx, &source_id, child_id).await? {
            continue;
        }

        // `before` is the disposition captured when the child was CREATED, not
        // the live row: the planner's edits land before it closes the child.
        let Some(at_creation) = read_source_snapshot(tx, &source_id, child_id).await? else {
            // No snapshot: a child created before 4etb shipped. There is no
            // honest `before` to compare against, so this close emits NO
            // outcome event and takes NO ownership branch — the conservative
            // direction, since the alternative is disposing of a source on
            // evidence nobody recorded. Such children drain within one
            // adjudication cycle; every child created from here on has one.
            continue;
        };
        let before = at_creation;
        let Some(at_close) = BusinessDisposition::read_tx(tx, &source_id).await? else {
            continue;
        };

        // ── Exhausted-ladder ownership ──────────────────────────────────────
        //
        // Only the UNCHANGED close of terminal round 3 invokes it. Rounds 1 and
        // 2 release into at most the next idempotent round, which is ordinary
        // scaffolding and leaves the disposition untouched. A round-3 close that
        // DID change the source (rescope, applied directive, supersede) is a
        // real disposition and must be left exactly as the planner left it —
        // disposing it here would destroy that decision.
        let round = planner_escalation_count_tx(tx, &source_id).await?;
        let changed_by_adjudication = !at_close.same_disposition_as(&before);
        // Did THIS transaction apply the exhausted-ladder ownership contract?
        //
        // Tracked explicitly rather than inferred from the before/after status
        // delta. The creation-time snapshot is the right `before` for the
        // planner's own edits (which land before the close), but it is the
        // WRONG baseline for a transition this transaction makes: a source the
        // producer snapshotted while it was already `pr_review` — the
        // merge-method wedge does exactly that — would compare `pr_review`
        // against `pr_review` and report `source_unchanged` for the very
        // handoff it just performed, contradicting outcome truth-table row 4.
        let mut ownership_applied = false;
        if round >= MAX_AUTONOMOUS_ESCALATIONS
            && !changed_by_adjudication
            && !already_owned(&at_close.status)
        {
            let (destination, close_reason) = match has_open_unmerged_pr(tx, &source_id).await? {
                // Owner: the PR poller (`poll_pr_review_tasks`).
                Some(_pr_url) => ("pr_review", None),
                // No poller required: terminal by contract.
                None => ("closed", Some(TERMINAL_CLOSE_REASON_CLASS)),
            };
            sqlx::query(
                r#"UPDATE tasks SET
                        status = $2,
                        close_reason = COALESCE($3, close_reason),
                        closed_at = CASE
                            WHEN $3::text IS NOT NULL
                            THEN to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                            ELSE closed_at
                        END,
                        updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                     WHERE id = $1"#,
            )
            .bind(&source_id)
            .bind(destination)
            .bind(close_reason)
            .execute(&mut **tx)
            .await?;

            // The disposition must be visible on the task timeline like any
            // other status change. The raw UPDATE above deliberately does not
            // go through `TaskRepository::transition` (it must commit inside
            // THIS transaction, alongside the child close), so the activity row
            // it would have written is written here instead.
            let activity_id = uuid::Uuid::now_v7().to_string();
            sqlx::query(
                "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload)
                 VALUES ($1, $2, $3, $4, 'status_changed', $5::jsonb)",
            )
            .bind(&activity_id)
            .bind(&source_id)
            .bind("coordinator")
            .bind("system")
            .bind(
                serde_json::json!({
                    "from_status": at_close.status,
                    "to_status": destination,
                    "reason": LADDER_EXHAUSTED_CLOSE_REASON,
                })
                .to_string(),
            )
            .execute(&mut **tx)
            .await?;
            ownership_applied = true;
        }

        // The adjudication has CLEARED for this source: its child is closed and
        // the blocker released. Drop the escalation evidence epoch so a LATER
        // trigger stamps a NEW one — otherwise every future escalation would
        // measure worker evidence against the first trigger's instant and count
        // attempts that belong to an adjudication already spent.
        //
        // This is deliberately NOT done on a guard-declined park: there the
        // escalation is still pending and the guards must keep measuring the
        // rotated redispatch against the SAME floor.
        sqlx::query("UPDATE tasks SET escalation_evidence_at = NULL WHERE id = $1")
            .bind(&source_id)
            .execute(&mut **tx)
            .await?;

        let after = BusinessDisposition::read_tx(tx, &source_id)
            .await?
            .unwrap_or_else(|| at_close.clone());

        let outcome = if ownership_applied || !after.same_disposition_as(&before) {
            SOURCE_CHANGED
        } else {
            SOURCE_UNCHANGED
        };
        let payload = serde_json::json!({
            "adjudication_child_id": child_id,
            "adjudication_outcome": outcome,
            "source_status_before": before.status,
            "source_status_after": after.status,
            "terminal_rung_round": round,
            // Operational metadata only — never an input to `adjudication_outcome`.
            "blocker_released": true,
        })
        .to_string();
        let activity_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(&activity_id)
        .bind(&source_id)
        .bind("coordinator")
        .bind("system")
        .bind(ADJUDICATION_OUTCOME_EVENT)
        .bind(&payload)
        .execute(&mut **tx)
        .await?;
        emitted += 1;
    }
    Ok(emitted)
}
