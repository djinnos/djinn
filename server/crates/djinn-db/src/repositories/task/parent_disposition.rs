// djinn:allow-oversize
//! Parent-terminal child disposition classification.
//!
//! When a parent container (epic or proposal) becomes terminal, its direct
//! child tasks must be classified into one of several outcomes:
//!
//! 1. **Close** — the child has no other open parent, no external open
//!    dependent, and is in a close-ready status bucket.
//! 2. **Park** — same preconditions as close, but the child is in an in-flight
//!    status bucket that requires lead reconciliation before a terminal move.
//! 3. **Retained (other open parent)** — the child is still owned by another
//!    open proposal that graduated the same epic (via `proposal_epics`).
//! 4. **Retained (external blocker)** — the child is still blocking at least
//!    one open task that lives *outside* the closing scope.
//! 5. **Already terminal** — the child is already `closed`; a no-op finding.
//!
//! This module performs **read-only classification** only. It does not mutate
//! any task or epic row. The returned [`DispositionPlan`] is an auditable plan
//! that a later transactional step (see epic f0ba, task mf0b) can apply
//! atomically before the parent's terminal state is committed.
//!
//! # Entry points
//!
//! - [`DispositionEntryPoint::EpicClose`] — direct epic close: scope is the
//!   single closing epic, `proposal_id` is `None`.
//!
//! Sibling epic `xc29` (proposal abort) is expected to reuse this helper with
//! additional entry points once those land; types are intentionally
//! entry-point-neutral.

use super::*;

// ── Types ────────────────────────────────────────────────────────────────────

/// Which terminal event triggered the disposition.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispositionEntryPoint {
    /// `EpicRepository::close` is the terminal trigger. The closing scope is a
    /// single epic id; `proposal_id` is `None` (direct epic close is not
    /// scoped to any particular proposal even when one graduated the epic).
    EpicClose,
    /// A proposal is being aborted; all of its linked epics are in scope.
    ProposalAbort,
    /// A proposal reconciliation marks one linked epic obsolete.
    ProposalObsoleteEpicReconcile,
}

/// The scope of a parent-terminal disposition request.
///
/// For `EpicClose` this is `{ entry_point: epic_close, epic_ids: [closing_epic_id], proposal_id: None }`.
#[derive(Clone, Debug)]
pub struct DispositionScope {
    /// Which terminal event triggered the request.
    pub entry_point: DispositionEntryPoint,
    /// Epic ids whose direct children are candidates for disposition.
    pub epic_ids: Vec<String>,
    /// The proposal being aborted, if any. `None` for direct epic close.
    pub proposal_id: Option<String>,
}

impl DispositionScope {
    /// Build the canonical scope for a direct epic close.
    ///
    /// `{ entry_point: epic_close, epic_ids: [closing_epic_id], proposal_id: None }`.
    pub fn for_epic_close(closing_epic_id: &str) -> Self {
        Self {
            entry_point: DispositionEntryPoint::EpicClose,
            epic_ids: vec![closing_epic_id.to_owned()],
            proposal_id: None,
        }
    }

    /// Build the canonical scope for aborting a proposal and its linked epics.
    /// The scoped proposal is terminal for the other-open-parent guard.
    pub fn for_proposal_abort(proposal_id: &str, epic_ids: Vec<String>) -> Self {
        Self {
            entry_point: DispositionEntryPoint::ProposalAbort,
            epic_ids,
            proposal_id: Some(proposal_id.to_owned()),
        }
    }

    /// Build the canonical scope for reconciling one obsolete proposal epic.
    pub fn for_proposal_obsolete_epic_reconcile(proposal_id: &str, epic_id: &str) -> Self {
        Self {
            entry_point: DispositionEntryPoint::ProposalObsoleteEpicReconcile,
            epic_ids: vec![epic_id.to_owned()],
            proposal_id: Some(proposal_id.to_owned()),
        }
    }
}

/// Guard reason captured for a retained child.
///
/// `close`/`park` findings carry no guard reason; these variants are only
/// populated when the child is retained (left unchanged).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardReason {
    /// Child belongs to at least one other *open* proposal that graduated the
    /// same epic. `excluded_proposal_id` is the proposal the current scope is
    /// aborting/closing through (if any); listed proposal ids are the open
    /// proposals that still claim the child's epic.
    OtherOpenProposalParent {
        open_proposals: Vec<OpenProposalRef>,
    },
    /// Child is still blocking at least one open task that lives outside the
    /// closing scope.
    ExternalOpenDependent { dependents: Vec<DependentRef> },
    /// Child is already terminal (`closed`). No action is needed.
    AlreadyTerminal,
}

/// Reference to an open proposal that still owns a child's epic.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct OpenProposalRef {
    pub proposal_id: String,
    pub status: String,
}

/// Reference to an external open task that depends on the child.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct DependentRef {
    pub task_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

/// The disposition outcome for a single child task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildDisposition {
    /// Close the child with `close_reason = parent_closed`.
    Close,
    /// Move the child to `needs_lead_intervention` (park) and keep
    /// `close_reason = NULL`.
    Park,
    /// Leave the child unchanged — it is already terminal.
    RetainedAlreadyTerminal,
    /// Leave the child unchanged — another open proposal still owns its epic.
    RetainedOtherParent,
    /// Leave the child unchanged — an external open task depends on it.
    RetainedExternalDependent,
}

impl ChildDisposition {
    /// `true` when this outcome applies a state change to the child.
    pub fn applies_change(&self) -> bool {
        matches!(self, Self::Close | Self::Park)
    }

    /// `true` when this outcome closes the child.
    pub fn closes(&self) -> bool {
        matches!(self, Self::Close)
    }

    /// `true` when this outcome parks the child.
    pub fn parks(&self) -> bool {
        matches!(self, Self::Park)
    }

    /// `true` when the child is left unchanged for any retention reason.
    pub fn retained(&self) -> bool {
        matches!(
            self,
            Self::RetainedAlreadyTerminal
                | Self::RetainedOtherParent
                | Self::RetainedExternalDependent
        )
    }
}

/// A single child's classification result.
#[derive(Clone, Debug)]
pub struct DispositionFinding {
    /// Child task id.
    pub task_id: String,
    /// Child short id (e.g. `a1b2`).
    pub short_id: String,
    /// Child title.
    pub title: String,
    /// Child status string at classification time.
    pub status: String,
    /// The normative guard that applied, if any.
    pub guard_reason: Option<GuardReason>,
    /// The final disposition outcome.
    pub disposition: ChildDisposition,
    /// Closing scope that produced this finding.
    pub scope: DispositionScopeEcho,
}

/// Lightweight echo of the scope that produced a finding, for later API
/// responses and tests.
#[derive(Clone, Debug)]
pub struct DispositionScopeEcho {
    pub entry_point: DispositionEntryPoint,
    pub epic_ids: Vec<String>,
    pub proposal_id: Option<String>,
}

impl From<&DispositionScope> for DispositionScopeEcho {
    fn from(scope: &DispositionScope) -> Self {
        Self {
            entry_point: scope.entry_point,
            epic_ids: scope.epic_ids.clone(),
            proposal_id: scope.proposal_id.clone(),
        }
    }
}

/// Aggregate counts of a [`DispositionPlan`], keyed by outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispositionCounts {
    pub close: usize,
    pub park: usize,
    pub retained_already_terminal: usize,
    pub retained_other_parent: usize,
    pub retained_external_dependent: usize,
}

/// The full read-only classification plan for one disposition scope.
#[derive(Clone, Debug)]
pub struct DispositionPlan {
    /// Scope that produced this plan.
    pub scope: DispositionScopeEcho,
    /// Per-child findings, in child `created_at` order.
    pub findings: Vec<DispositionFinding>,
    /// Aggregate outcome counts.
    pub counts: DispositionCounts,
}

// ── Status bucketing ─────────────────────────────────────────────────────────

/// Close-ready statuses: the child has no in-flight work and can be closed.
///
/// These are the snake_case DB wire strings for `TaskStatus::Open`,
/// `TaskStatus::NeedsLeadIntervention`, and `TaskStatus::InLeadIntervention`
/// (see `TaskStatus::as_str`). Kept as literals so the array can be `const`.
pub(crate) const CLOSE_READY_STATUSES: &[&str] =
    &["open", "needs_lead_intervention", "in_lead_intervention"];

/// Park statuses: the child has in-flight work (review/PR) that requires lead
/// reconciliation before a terminal move.
///
/// These are the snake_case DB wire strings for `in_progress`,
/// `needs_task_review`, `in_task_review`, `approved`, `pr_draft`, and
/// `pr_review` (see `TaskStatus::as_str`).
pub(crate) const PARK_STATUSES: &[&str] = &[
    "in_progress",
    "needs_task_review",
    "in_task_review",
    "approved",
    "pr_draft",
    "pr_review",
];

/// Proposal statuses considered *terminal* — no longer an open parent.
///
/// This documents the same set hard-coded in the
/// `find_other_open_proposal_parents` SQL `NOT IN (...)` clause; kept as a
/// named constant so the invariant is legible and testable.
#[allow(dead_code)]
pub(crate) const TERMINAL_PROPOSAL_STATUSES: &[&str] =
    &["archived", "superseded", "rejected", "done"];

// ── Repository impl ──────────────────────────────────────────────────────────

impl TaskRepository {
    /// Classify every non-closed direct child of the closing scope's epic(s)
    /// into a disposition outcome. Read-only: mutates nothing.
    ///
    /// Normative guard order applied per child:
    ///   1. already-terminal child
    ///   2. other-open-proposal-parent guard (`proposal_epics`)
    ///   3. external-open-dependent guard (`blockers`)
    ///   4. status matrix (close-ready → `Close`, park → `Park`)
    ///
    /// For [`DispositionEntryPoint::EpicClose`], the excluded proposal is
    /// `None`, so *every* open proposal that graduated the child's epic counts
    /// as another open parent.
    pub async fn classify_parent_disposition(
        &self,
        scope: &DispositionScope,
    ) -> Result<DispositionPlan> {
        self.db.ensure_initialized().await?;
        if scope.epic_ids.is_empty() {
            return Ok(DispositionPlan {
                scope: DispositionScopeEcho::from(scope),
                findings: vec![],
                counts: DispositionCounts::default(),
            });
        }

        let children = self.select_candidate_children(scope).await?;
        let echo = DispositionScopeEcho::from(scope);

        let mut findings = Vec::with_capacity(children.len());
        for child in &children {
            let disposition = self.classify_child(child, scope).await?;
            let guard_reason = guard_reason_for_disposition(&disposition);

            // Lazy-fill evidence for retained findings so callers/tests have
            // enough IDs/reasons without a second pass.
            let guard_reason = match (disposition.clone(), guard_reason) {
                (ChildDisposition::RetainedOtherParent, _) => {
                    let open_proposals = self
                        .find_other_open_proposal_parents(&child.task_id, scope)
                        .await?;
                    Some(GuardReason::OtherOpenProposalParent { open_proposals })
                }
                (ChildDisposition::RetainedExternalDependent, _) => {
                    let dependents = self
                        .find_external_open_dependents(&child.task_id, scope)
                        .await?;
                    Some(GuardReason::ExternalOpenDependent { dependents })
                }
                (ChildDisposition::RetainedAlreadyTerminal, _) => {
                    Some(GuardReason::AlreadyTerminal)
                }
                (other, existing) => match other {
                    ChildDisposition::Close | ChildDisposition::Park => None,
                    _ => existing,
                },
            };

            findings.push(DispositionFinding {
                task_id: child.task_id.clone(),
                short_id: child.short_id.clone(),
                title: child.title.clone(),
                status: child.status.clone(),
                guard_reason,
                disposition,
                scope: echo.clone(),
            });
        }

        let counts = tally(&findings);

        Ok(DispositionPlan {
            scope: echo,
            findings,
            counts,
        })
    }

    // ── Child selection ────────────────────────────────────────────────────

    /// Select direct children of the closing epic(s) that are *not already
    /// closed*, plus any already-terminal children (so the plan records the
    /// no-op finding for already-closed tasks as well). Rows are ordered by
    /// `created_at` for stable, auditable output.
    async fn select_candidate_children(
        &self,
        scope: &DispositionScope,
    ) -> Result<Vec<CandidateChild>> {
        // Runtime query: the candidate set depends on the dynamic `epic_ids`
        // array (`= ANY($1)`), which the compile-time `query!`/`query_as!`
        // macros cannot bind without a pre-recorded `.sqlx` entry. Mirrors the
        // established runtime-query pattern used elsewhere for `ANY` binds
        // (e.g. `task_run.rs`, `dispatch_state.rs`).
        let rows = sqlx::query_as::<_, CandidateChild>(
            r#"SELECT id AS task_id, short_id, title, status, epic_id
                 FROM tasks
                WHERE epic_id = ANY($1)
                ORDER BY created_at"#,
        )
        .bind(&scope.epic_ids)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    // ── Guard 2: other-open-proposal-parent ────────────────────────────────

    /// Find open proposals (other than the scope's excluded one) that graduated
    /// the child's epic. Returns the proposals that still claim ownership.
    async fn find_other_open_proposal_parents(
        &self,
        child_task_id: &str,
        scope: &DispositionScope,
    ) -> Result<Vec<OpenProposalRef>> {
        // The child's epic_id is the join key into proposal_epics. We exclude
        // the scope's own aborting proposal (if any) and any terminal proposal
        // status.
        let excluded = scope.proposal_id.as_deref();
        let rows = sqlx::query_as::<_, OpenProposalRef>(
            r#"SELECT p.id AS proposal_id, p.status AS status
                 FROM proposal_epics pe
                 JOIN proposals p ON p.id = pe.proposal_id
                 JOIN tasks t ON t.id = $1 AND t.epic_id = pe.epic_id
                WHERE p.status NOT IN ('archived', 'superseded', 'rejected', 'done')
                  AND ($2::text IS NULL OR p.id <> $2)
                ORDER BY p.id"#,
        )
        .bind(child_task_id)
        .bind(excluded)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    /// `true` when at least one other open proposal owns the child's epic.
    async fn has_other_open_proposal_parent(
        &self,
        child_task_id: &str,
        scope: &DispositionScope,
    ) -> Result<bool> {
        Ok(!self
            .find_other_open_proposal_parents(child_task_id, scope)
            .await?
            .is_empty())
    }

    // ── Guard 3: external-open-dependent ───────────────────────────────────

    /// Find open tasks that depend on (are blocked by) the candidate child and
    /// live *outside* the closing epic scope. Dependents inside the same
    /// closing epic are internal and must not block disposition.
    async fn find_external_open_dependents(
        &self,
        child_task_id: &str,
        scope: &DispositionScope,
    ) -> Result<Vec<DependentRef>> {
        // Candidate child is blockers.blocking_task_id; open dependents are
        // blockers.task_id rows whose task status is not 'closed'. Internal
        // dependents (same closing epic) are excluded.
        let rows = sqlx::query_as::<_, DependentRef>(
            r#"SELECT t.id AS task_id, t.short_id, t.title, t.status
                 FROM blockers b
                 JOIN tasks t ON t.id = b.task_id
                WHERE b.blocking_task_id = $1
                  AND t.status <> 'closed'
                  AND (t.epic_id IS NULL OR t.epic_id <> ALL($2))
                ORDER BY t.created_at"#,
        )
        .bind(child_task_id)
        .bind(&scope.epic_ids)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    /// `true` when at least one external open task depends on the candidate
    /// child.
    async fn has_external_open_dependent(
        &self,
        child_task_id: &str,
        scope: &DispositionScope,
    ) -> Result<bool> {
        Ok(!self
            .find_external_open_dependents(child_task_id, scope)
            .await?
            .is_empty())
    }

    // ── Per-child classification ───────────────────────────────────────────

    /// Apply the normative guard order to a single candidate child.
    async fn classify_child(
        &self,
        child: &CandidateChild,
        scope: &DispositionScope,
    ) -> Result<ChildDisposition> {
        // Guard 1: already-terminal child — no action.
        if is_terminal_task_status(&child.status) {
            return Ok(ChildDisposition::RetainedAlreadyTerminal);
        }

        // Guard 2: other-open-proposal-parent — retained.
        if self
            .has_other_open_proposal_parent(&child.task_id, scope)
            .await?
        {
            return Ok(ChildDisposition::RetainedOtherParent);
        }

        // Guard 3: external-open-dependent — retained.
        if self
            .has_external_open_dependent(&child.task_id, scope)
            .await?
        {
            return Ok(ChildDisposition::RetainedExternalDependent);
        }

        // Guard 4: status matrix.
        if CLOSE_READY_STATUSES.contains(&child.status.as_str()) {
            return Ok(ChildDisposition::Close);
        }
        if PARK_STATUSES.contains(&child.status.as_str()) {
            return Ok(ChildDisposition::Park);
        }

        // Unknown / unmapped status: retain conservatively rather than guess.
        Ok(ChildDisposition::RetainedAlreadyTerminal)
    }
}

// ── Transactional re-classification ─────────────────────────────────────────

/// Re-classify a single child under an active database transaction.
///
/// Re-runs the full normative guard order using the transaction connection so
/// that guards 2 (other-open-parent) and 3 (external-dependent) are checked
/// under the same row lock that protects the mutation. This closes the TOCTOU
/// gap between read-time classification and mutation-time application that
/// exists when [`TaskRepository::classify_parent_disposition`] is called
/// outside the transaction.
///
/// Call this after locking the child row with `FOR UPDATE` and confirming the
/// status has not changed; pass the `current_status` obtained from the lock.
pub async fn classify_child_tx(
    conn: &mut sqlx::PgConnection,
    task_id: &str,
    status: &str,
    scope: &DispositionScope,
) -> Result<ChildDisposition> {
    // Guard 1: already-terminal child — no action.
    if is_terminal_task_status(status) {
        return Ok(ChildDisposition::RetainedAlreadyTerminal);
    }

    // Guard 2: other-open-proposal-parent — retained.
    // The child's epic_id is the join key into proposal_epics. We exclude the
    // scope's own aborting proposal (if any) and any terminal proposal status.
    let excluded = scope.proposal_id.as_deref();
    let other_parent_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
             FROM proposal_epics pe
             JOIN proposals p ON p.id = pe.proposal_id
             JOIN tasks t ON t.id = $1 AND t.epic_id = pe.epic_id
            WHERE p.status NOT IN ('archived', 'superseded', 'rejected', 'done')
              AND ($2::text IS NULL OR p.id <> $2)"#,
    )
    .bind(task_id)
    .bind(excluded)
    .fetch_one(&mut *conn)
    .await?;

    if other_parent_count > 0 {
        return Ok(ChildDisposition::RetainedOtherParent);
    }

    // Guard 3: external-open-dependent — retained.
    // Candidate child is blockers.blocking_task_id; open dependents are
    // blockers.task_id rows whose task status is not 'closed'. Internal
    // dependents (same closing epic) are excluded.
    let external_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*)
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
            WHERE b.blocking_task_id = $1
              AND t.status <> 'closed'
              AND (t.epic_id IS NULL OR t.epic_id <> ALL($2))"#,
    )
    .bind(task_id)
    .bind(&scope.epic_ids)
    .fetch_one(&mut *conn)
    .await?;

    if external_count > 0 {
        return Ok(ChildDisposition::RetainedExternalDependent);
    }

    // Guard 4: status matrix.
    if CLOSE_READY_STATUSES.contains(&status) {
        return Ok(ChildDisposition::Close);
    }
    if PARK_STATUSES.contains(&status) {
        return Ok(ChildDisposition::Park);
    }

    // Unknown / unmapped status: retain conservatively rather than guess.
    Ok(ChildDisposition::RetainedAlreadyTerminal)
}

/// Collect evidence for the transaction-time other-open-parent guard.
async fn find_other_open_proposal_parents_tx(
    conn: &mut sqlx::PgConnection,
    task_id: &str,
    scope: &DispositionScope,
) -> Result<Vec<OpenProposalRef>> {
    Ok(sqlx::query_as::<_, OpenProposalRef>(
        r#"SELECT p.id AS proposal_id, p.status AS status
             FROM proposal_epics pe
             JOIN proposals p ON p.id = pe.proposal_id
             JOIN tasks t ON t.id = $1 AND t.epic_id = pe.epic_id
            WHERE p.status NOT IN ('archived', 'superseded', 'rejected', 'done')
              AND ($2::text IS NULL OR p.id <> $2)
            ORDER BY p.id"#,
    )
    .bind(task_id)
    .bind(scope.proposal_id.as_deref())
    .fetch_all(&mut *conn)
    .await?)
}

/// Collect evidence for the transaction-time external-dependent guard.
async fn find_external_open_dependents_tx(
    conn: &mut sqlx::PgConnection,
    task_id: &str,
    scope: &DispositionScope,
) -> Result<Vec<DependentRef>> {
    sqlx::query_as::<_, DependentRef>(
        r#"SELECT t.id AS task_id, t.short_id, t.title, t.status
             FROM blockers b
             JOIN tasks t ON t.id = b.task_id
            WHERE b.blocking_task_id = $1
              AND t.status <> 'closed'
              AND (t.epic_id IS NULL OR t.epic_id <> ALL($2))
            ORDER BY t.created_at"#,
    )
    .bind(task_id)
    .bind(&scope.epic_ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(Into::into)
}

/// Apply parent-terminal child disposition inside an existing transaction.
///
/// Candidate rows are selected and individually locked before being classified
/// and mutated, so callers can safely combine this with their own parent state
/// transition. Retained children are recorded in the returned plan and never
/// cause the transaction to fail merely because a guard applied.
pub async fn apply_parent_disposition_tx(
    conn: &mut sqlx::PgConnection,
    scope: &DispositionScope,
) -> Result<DispositionPlan> {
    let echo = DispositionScopeEcho::from(scope);
    let child_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM tasks WHERE epic_id = ANY($1) ORDER BY created_at")
            .bind(&scope.epic_ids)
            .fetch_all(&mut *conn)
            .await?;
    let mut findings = Vec::with_capacity(child_ids.len());
    let entry_point = match scope.entry_point {
        DispositionEntryPoint::EpicClose => "epic_close",
        DispositionEntryPoint::ProposalAbort => "proposal_abort",
        DispositionEntryPoint::ProposalObsoleteEpicReconcile => "proposal_obsolete_epic_reconcile",
    };
    let parent_kind = if scope.proposal_id.is_some() {
        "proposal"
    } else {
        "epic"
    };
    let parent_id = scope
        .proposal_id
        .as_deref()
        .or_else(|| scope.epic_ids.first().map(String::as_str))
        .unwrap_or_default();

    for task_id in child_ids {
        let child: Option<CandidateChild> = sqlx::query_as(
            "SELECT id AS task_id, short_id, title, status, epic_id FROM tasks WHERE id = $1 FOR UPDATE",
        )
        .bind(&task_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(child) = child else { continue };
        let disposition = classify_child_tx(conn, &child.task_id, &child.status, scope).await?;
        // Populate retention evidence from the same transaction that made the
        // classification decision. Besides making the returned plan useful to
        // terminal callers, this keeps the evidence coherent with the locked
        // child row and avoids a read-after-commit race.
        let guard_reason = match disposition {
            ChildDisposition::RetainedOtherParent => Some(GuardReason::OtherOpenProposalParent {
                open_proposals: find_other_open_proposal_parents_tx(conn, &child.task_id, scope)
                    .await?,
            }),
            ChildDisposition::RetainedExternalDependent => {
                Some(GuardReason::ExternalOpenDependent {
                    dependents: find_external_open_dependents_tx(conn, &child.task_id, scope)
                        .await?,
                })
            }
            _ => guard_reason_for_disposition(&disposition),
        };

        match disposition {
            ChildDisposition::Close => {
                sqlx::query(r#"UPDATE tasks SET status = 'closed',
                    closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    close_reason = 'parent_closed',
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#)
                    .bind(&child.task_id).execute(&mut *conn).await?;
                insert_disposition_activities(
                    conn,
                    &child.task_id,
                    &child.status,
                    "closed",
                    "parent_closed",
                    "parent_child_disposed",
                    parent_kind,
                    parent_id,
                    entry_point,
                )
                .await?;
            }
            ChildDisposition::Park => {
                let reason = park_reason_for_status(&child.status);
                sqlx::query(r#"UPDATE tasks SET status = 'needs_lead_intervention',
                    intervention_count = intervention_count + 1,
                    last_intervention_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    close_reason = NULL,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#)
                    .bind(&child.task_id).execute(&mut *conn).await?;
                insert_disposition_activities(
                    conn,
                    &child.task_id,
                    &child.status,
                    "needs_lead_intervention",
                    reason,
                    "parent_child_parked",
                    parent_kind,
                    parent_id,
                    entry_point,
                )
                .await?;
            }
            _ => {}
        }
        findings.push(DispositionFinding {
            task_id: child.task_id,
            short_id: child.short_id,
            title: child.title,
            status: child.status,
            guard_reason,
            disposition,
            scope: echo.clone(),
        });
    }
    let counts = tally(&findings);
    Ok(DispositionPlan {
        scope: echo,
        findings,
        counts,
    })
}

// ── Doctor repair (historical closed-parent orphan) ──────────────────────────

/// Outcome of a single-task doctor repair attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DoctorRepairOutcome {
    /// The task was closed with `close_reason = parent_closed`.
    Closed {
        task_id: String,
        from_status: String,
    },
    /// The task was parked to `needs_lead_intervention` with a historical
    /// repair reason.
    Parked {
        task_id: String,
        from_status: String,
        reason: &'static str,
    },
    /// The repair was skipped because the task's status has drifted since the
    /// finding snapshot was captured.
    SkippedStatusDrift {
        task_id: String,
        snapshot_status: String,
        current_status: String,
    },
    /// The repair was skipped because the task is already closed.
    SkippedAlreadyClosed { task_id: String },
    /// The repair was skipped because another open parent now exists.
    SkippedOtherOpenParent { task_id: String },
    /// The repair was skipped because an external open dependent now exists.
    SkippedExternalDependent { task_id: String },
    /// The repair was skipped because the finding's recommended action was
    /// `retain` (guarded row).
    SkippedRetain { task_id: String },
    /// The task row was not found (deleted between finding and repair).
    SkippedNotFound { task_id: String },
}

impl DoctorRepairOutcome {
    /// `true` when the repair applied a mutation to the task.
    pub fn applied(&self) -> bool {
        matches!(self, Self::Closed { .. } | Self::Parked { .. })
    }
}

/// Apply the opt-in mutating doctor repair for one closed-parent orphan child
/// inside a caller-provided transaction.
///
/// This is the transactional repair counterpart to the read-only
/// [`classify_parent_disposition`] / board-health finding. It **re-locks** the
/// task row (`SELECT … FOR UPDATE`), compares the persisted finding snapshot,
/// and re-runs [`classify_child_tx`] under the same lock so stale findings
/// cannot mutate rows whose state changed after the snapshot.
///
/// # Scope
///
/// `terminal_epic_ids` and `terminal_proposal_ids` are taken verbatim from the
/// board-health finding. The repair reconstructs the full disposition scope by
/// including every epic linked to a terminal proposal, so the external-dependent
/// guard treats siblings in the same proposal scope as internal.
///
/// # Guards (all checked under the row lock)
///
/// 1. **Not found** — the task row was deleted → `SkippedNotFound`.
/// 2. **Already closed** — current status is `closed` → `SkippedAlreadyClosed`.
/// 3. **Retain finding** — the snapshot's recommended action was `retain` →
///    `SkippedRetain`.
/// 4. **Status drift** — the current status differs from `snapshot_status`
///    (the finding is stale) → `SkippedStatusDrift`.
/// 5. **Other open parent** — `classify_child_tx` returns
///    `RetainedOtherParent` → `SkippedOtherOpenParent`.
/// 6. **External open dependent** — `classify_child_tx` returns
///    `RetainedExternalDependent` → `SkippedExternalDependent`.
///
/// # Mutations
///
/// - **Close-ready** (`open`, `needs_lead_intervention`, `in_lead_intervention`):
///   sets `status = 'closed'`, `close_reason = 'parent_closed'`.
/// - **Park** (`in_progress`, `needs_task_review`, `in_task_review`, `approved`,
///   `pr_draft`, `pr_review`): sets `status = 'needs_lead_intervention'`,
///   increments `intervention_count`, uses the `historical_*` park reason.
///
/// Both paths emit a normal `status_changed` activity and a doctor-specific
/// audit activity with `{ source: doctor_fix, check: closed_parent_open_children,
/// original_parent_ids }`. When parking, PR identity and live session evidence
/// are preserved as separate audit activities.
pub async fn apply_doctor_repair_tx(
    conn: &mut sqlx::PgConnection,
    task_id: &str,
    snapshot_status: &str,
    snapshot_action: &str,
    terminal_epic_ids: &[String],
    terminal_proposal_ids: &[String],
) -> Result<DoctorRepairOutcome> {
    // Lock the task row for the duration of the repair so no concurrent
    // transition can race us between the re-check and the mutation.
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, pr_url FROM tasks WHERE id = $1 FOR UPDATE")
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?;

    let Some((current_status, pr_url)) = row else {
        return Ok(DoctorRepairOutcome::SkippedNotFound {
            task_id: task_id.to_owned(),
        });
    };

    // Guard: already closed — nothing to do.
    if current_status == "closed" {
        return Ok(DoctorRepairOutcome::SkippedAlreadyClosed {
            task_id: task_id.to_owned(),
        });
    }

    // Guard: the finding itself was a retain (guarded row). We never mutate
    // guarded rows even if the snapshot is stale.
    if snapshot_action == "retain" {
        return Ok(DoctorRepairOutcome::SkippedRetain {
            task_id: task_id.to_owned(),
        });
    }

    // Guard: status drift. The persisted finding snapshot must still describe
    // the locked row; any mismatch means the finding is stale.
    if current_status != snapshot_status {
        return Ok(DoctorRepairOutcome::SkippedStatusDrift {
            task_id: task_id.to_owned(),
            snapshot_status: snapshot_status.to_owned(),
            current_status,
        });
    }

    // Re-run the shared classifier under the lock. The repair scope uses the
    // terminal epic ids from the finding plus every epic linked to a terminal
    // proposal, so the external-dependent guard treats dependents inside the
    // same proposal scope as internal.
    let mut scope_epic_ids: Vec<String> = terminal_epic_ids.to_vec();
    if !terminal_proposal_ids.is_empty() {
        let linked: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT epic_id FROM proposal_epics WHERE proposal_id = ANY($1)",
        )
        .bind(terminal_proposal_ids)
        .fetch_all(&mut *conn)
        .await?;
        for id in linked {
            if !scope_epic_ids.contains(&id) {
                scope_epic_ids.push(id);
            }
        }
    }
    if scope_epic_ids.is_empty() {
        let epic_id: Option<String> = sqlx::query_scalar("SELECT epic_id FROM tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&mut *conn)
            .await?;
        if let Some(epic_id) = epic_id {
            scope_epic_ids.push(epic_id);
        }
    }
    let scope = if let Some(proposal_id) = terminal_proposal_ids.first() {
        DispositionScope::for_proposal_abort(proposal_id, scope_epic_ids)
    } else {
        DispositionScope::for_epic_close(&scope_epic_ids[0])
    };

    let original_parent_ids: Vec<String> = terminal_epic_ids
        .iter()
        .chain(terminal_proposal_ids.iter())
        .cloned()
        .collect();

    let disposition = classify_child_tx(conn, task_id, &current_status, &scope).await?;

    match disposition {
        ChildDisposition::RetainedAlreadyTerminal => {
            Ok(DoctorRepairOutcome::SkippedAlreadyClosed {
                task_id: task_id.to_owned(),
            })
        }
        ChildDisposition::RetainedOtherParent => Ok(DoctorRepairOutcome::SkippedOtherOpenParent {
            task_id: task_id.to_owned(),
        }),
        ChildDisposition::RetainedExternalDependent => {
            Ok(DoctorRepairOutcome::SkippedExternalDependent {
                task_id: task_id.to_owned(),
            })
        }
        ChildDisposition::Close => {
            sqlx::query(
                r#"UPDATE tasks SET status = 'closed',
                    closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    close_reason = 'parent_closed',
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#,
            )
            .bind(task_id)
            .execute(&mut *conn)
            .await?;

            insert_doctor_repair_activities(
                conn,
                task_id,
                &current_status,
                "closed",
                "parent_closed",
                &original_parent_ids,
                None,
                None,
            )
            .await?;

            Ok(DoctorRepairOutcome::Closed {
                task_id: task_id.to_owned(),
                from_status: current_status,
            })
        }
        ChildDisposition::Park => {
            let reason = historical_park_reason_for_status(&current_status);
            sqlx::query(
                r#"UPDATE tasks SET status = 'needs_lead_intervention',
                    intervention_count = intervention_count + 1,
                    last_intervention_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    close_reason = NULL,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#,
            )
            .bind(task_id)
            .execute(&mut *conn)
            .await?;

            // Capture the live session (if any) before emitting the repair audit
            // so the doctor_fix_repair payload can include it alongside the PR.
            let active_session: Option<String> = sqlx::query_scalar(
                "SELECT id FROM sessions WHERE task_id = $1 AND status = 'running' \
                 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(task_id)
            .fetch_optional(&mut *conn)
            .await?;

            insert_doctor_repair_activities(
                conn,
                task_id,
                &current_status,
                "needs_lead_intervention",
                reason,
                &original_parent_ids,
                active_session.clone(),
                pr_url.clone(),
            )
            .await?;

            // Preserve PR identity evidence: if the task had a pr_url, record
            // it in the audit payload so the lead can reconcile the live PR.
            if let Some(pr_url) = pr_url {
                let audit_payload = serde_json::json!({
                    "source": "doctor_fix",
                    "check": "closed_parent_open_children",
                    "preserved_pr_url": pr_url,
                    "park_reason": reason,
                });
                sqlx::query(
                    "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) \
                     VALUES ($1, $2, 'system', 'system', 'doctor_fix_pr_preserved', $3)",
                )
                .bind(uuid::Uuid::now_v7().to_string())
                .bind(task_id)
                .bind(&audit_payload)
                .execute(&mut *conn)
                .await?;
            }

            // Preserve live session identity: record any active session so the
            // lead knows the task was mid-flight when parked.
            if let Some(session_id) = active_session {
                let audit_payload = serde_json::json!({
                    "source": "doctor_fix",
                    "check": "closed_parent_open_children",
                    "preserved_session_id": session_id,
                    "park_reason": reason,
                });
                sqlx::query(
                    "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) \
                     VALUES ($1, $2, 'system', 'system', 'doctor_fix_session_preserved', $3)",
                )
                .bind(uuid::Uuid::now_v7().to_string())
                .bind(task_id)
                .bind(&audit_payload)
                .execute(&mut *conn)
                .await?;
            }

            Ok(DoctorRepairOutcome::Parked {
                task_id: task_id.to_owned(),
                from_status: current_status,
                reason,
            })
        }
    }
}

/// Emit the normal `status_changed` activity and the doctor-specific audit
/// evidence for a repair mutation.
#[allow(clippy::too_many_arguments)]
async fn insert_doctor_repair_activities(
    conn: &mut sqlx::PgConnection,
    task_id: &str,
    from_status: &str,
    to_status: &str,
    reason: &str,
    original_parent_ids: &[String],
    preserved_session_id: Option<String>,
    preserved_pr_url: Option<String>,
) -> Result<()> {
    // Normal task status-change activity (same shape as lifecycle transitions).
    let status_payload = serde_json::json!({
        "from_status": from_status,
        "to_status": to_status,
        "reason": reason,
    });
    sqlx::query(
        "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) \
         VALUES ($1, $2, 'system', 'system', 'status_changed', $3)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(task_id)
    .bind(&status_payload)
    .execute(&mut *conn)
    .await?;

    // Doctor-specific audit evidence.
    let mut audit_payload = serde_json::json!({
        "source": "doctor_fix",
        "check": "closed_parent_open_children",
        "original_parent_ids": original_parent_ids,
        "from_status": from_status,
        "to_status": to_status,
        "reason": reason,
    });
    if to_status == "needs_lead_intervention" {
        audit_payload["park_reason"] = serde_json::Value::String(reason.to_owned());
    }
    if let Some(session_id) = preserved_session_id {
        audit_payload["preserved_session_id"] = serde_json::Value::String(session_id);
    }
    if let Some(pr_url) = preserved_pr_url {
        audit_payload["preserved_pr_url"] = serde_json::Value::String(pr_url);
    }
    sqlx::query(
        "INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) \
         VALUES ($1, $2, 'system', 'system', 'doctor_fix_repair', $3)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(task_id)
    .bind(&audit_payload)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_disposition_activities(
    conn: &mut sqlx::PgConnection,
    task_id: &str,
    from_status: &str,
    to_status: &str,
    reason: &str,
    event_type: &str,
    parent_kind: &str,
    parent_id: &str,
    entry_point: &str,
) -> Result<()> {
    let status_payload =
        serde_json::json!({"from_status": from_status, "to_status": to_status, "reason": reason});
    let mut disposition_payload = serde_json::json!({"parent_kind": parent_kind, "parent_id": parent_id, "entry_point": entry_point, "from_status": from_status, "to_status": to_status, "reason": reason});
    // `park_reason` is the established parent-child activity contract. Keep
    // the generic reason too: status_changed continues to use that key.
    if event_type == "parent_child_parked" {
        disposition_payload["park_reason"] = serde_json::Value::String(reason.to_owned());
    }
    for (event_type, payload) in [
        ("status_changed", status_payload),
        (event_type, disposition_payload),
    ] {
        sqlx::query("INSERT INTO activity_log (id, task_id, actor_id, actor_role, event_type, payload) VALUES ($1, $2, 'system', 'system', $3, $4)")
            .bind(uuid::Uuid::now_v7().to_string()).bind(task_id).bind(event_type).bind(payload)
            .execute(&mut *conn).await?;
    }
    Ok(())
}

fn park_reason_for_status(status: &str) -> &'static str {
    match status {
        "approved" | "pr_draft" | "pr_review" => "parent_closed_pr_active",
        _ => "parent_closed_in_flight",
    }
}

/// Repair-specific park reason for historical closed-parent orphan repair.
/// Mirrors [`park_reason_for_status`] but uses the `historical_*` namespace so
/// doctor_fix can distinguish historical drift from fresh epic-close cascades.
pub(crate) fn historical_park_reason_for_status(status: &str) -> &'static str {
    match status {
        "approved" | "pr_draft" | "pr_review" => "historical_parent_closed_pr_active",
        _ => "historical_parent_closed_in_flight",
    }
}

// ── Internal row + helpers ───────────────────────────────────────────────────

/// Raw candidate-child row projected by `select_candidate_children`.
#[derive(Debug, sqlx::FromRow)]
struct CandidateChild {
    task_id: String,
    short_id: String,
    title: String,
    status: String,
    /// Carried so the classification can confirm epic membership; not surfaced
    /// in findings.
    #[allow(dead_code)]
    epic_id: Option<String>,
}

/// Map a disposition outcome to its guard reason tag (without evidence).
fn guard_reason_for_disposition(disposition: &ChildDisposition) -> Option<GuardReason> {
    match disposition {
        ChildDisposition::RetainedAlreadyTerminal => Some(GuardReason::AlreadyTerminal),
        // Evidence (proposals/dependents) is filled in lazily by the caller.
        ChildDisposition::RetainedOtherParent => None,
        ChildDisposition::RetainedExternalDependent => None,
        ChildDisposition::Close | ChildDisposition::Park => None,
    }
}

/// Tally findings into aggregate outcome counts.
fn tally(findings: &[DispositionFinding]) -> DispositionCounts {
    let mut counts = DispositionCounts::default();
    for f in findings {
        match f.disposition {
            ChildDisposition::Close => counts.close += 1,
            ChildDisposition::Park => counts.park += 1,
            ChildDisposition::RetainedAlreadyTerminal => counts.retained_already_terminal += 1,
            ChildDisposition::RetainedOtherParent => counts.retained_other_parent += 1,
            ChildDisposition::RetainedExternalDependent => counts.retained_external_dependent += 1,
        }
    }
    counts
}

/// `true` when the task status string is terminal (`closed`).
fn is_terminal_task_status(status: &str) -> bool {
    status == TaskStatus::Closed.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure-logic status-bucket tests ─────────────────────────────────────

    #[test]
    fn close_ready_statuses_match_spec() {
        // Spec: close-ready = open, needs_lead_intervention, in_lead_intervention.
        assert!(CLOSE_READY_STATUSES.contains(&TaskStatus::Open.as_str()));
        assert!(CLOSE_READY_STATUSES.contains(&TaskStatus::NeedsLeadIntervention.as_str()));
        assert!(CLOSE_READY_STATUSES.contains(&TaskStatus::InLeadIntervention.as_str()));
        // Park buckets must NOT be in close-ready.
        assert!(!CLOSE_READY_STATUSES.contains(&TaskStatus::InProgress.as_str()));
        assert!(!CLOSE_READY_STATUSES.contains(&TaskStatus::PrReview.as_str()));
    }

    #[test]
    fn park_statuses_match_spec() {
        // Spec: park = in_progress, needs_task_review, in_task_review, approved,
        // pr_draft, pr_review.
        assert!(PARK_STATUSES.contains(&TaskStatus::InProgress.as_str()));
        assert!(PARK_STATUSES.contains(&TaskStatus::NeedsTaskReview.as_str()));
        assert!(PARK_STATUSES.contains(&TaskStatus::InTaskReview.as_str()));
        assert!(PARK_STATUSES.contains(&TaskStatus::Approved.as_str()));
        assert!(PARK_STATUSES.contains(&TaskStatus::PrDraft.as_str()));
        assert!(PARK_STATUSES.contains(&TaskStatus::PrReview.as_str()));
        // Close-ready buckets must NOT be park.
        assert!(!PARK_STATUSES.contains(&TaskStatus::Open.as_str()));
        assert!(!PARK_STATUSES.contains(&TaskStatus::Closed.as_str()));
    }

    #[test]
    fn is_terminal_recognises_closed() {
        assert!(is_terminal_task_status("closed"));
        assert!(!is_terminal_task_status("open"));
        assert!(!is_terminal_task_status("in_progress"));
    }

    #[test]
    fn terminal_proposal_statuses_are_non_open() {
        assert!(TERMINAL_PROPOSAL_STATUSES.contains(&"done"));
        assert!(TERMINAL_PROPOSAL_STATUSES.contains(&"archived"));
        assert!(!TERMINAL_PROPOSAL_STATUSES.contains(&"building"));
        assert!(!TERMINAL_PROPOSAL_STATUSES.contains(&"draft"));
    }

    #[test]
    fn scope_for_epic_close_matches_contract() {
        let scope = DispositionScope::for_epic_close("epic-1");
        assert_eq!(scope.entry_point, DispositionEntryPoint::EpicClose);
        assert_eq!(scope.epic_ids, vec!["epic-1".to_owned()]);
        assert_eq!(scope.proposal_id, None);
    }

    #[test]
    fn child_disposition_predicates() {
        assert!(ChildDisposition::Close.applies_change());
        assert!(ChildDisposition::Close.closes());
        assert!(!ChildDisposition::Close.parks());
        assert!(!ChildDisposition::Close.retained());

        assert!(ChildDisposition::Park.applies_change());
        assert!(!ChildDisposition::Park.closes());
        assert!(ChildDisposition::Park.parks());
        assert!(!ChildDisposition::Park.retained());

        for d in [
            ChildDisposition::RetainedAlreadyTerminal,
            ChildDisposition::RetainedOtherParent,
            ChildDisposition::RetainedExternalDependent,
        ] {
            assert!(!d.applies_change());
            assert!(d.retained());
        }
    }

    #[test]
    fn tally_counts_each_outcome() {
        let mk = |disposition: ChildDisposition| DispositionFinding {
            task_id: "t".to_owned(),
            short_id: "s".to_owned(),
            title: "x".to_owned(),
            status: "open".to_owned(),
            guard_reason: None,
            disposition,
            scope: DispositionScopeEcho {
                entry_point: DispositionEntryPoint::EpicClose,
                epic_ids: vec!["e".to_owned()],
                proposal_id: None,
            },
        };
        let findings = vec![
            mk(ChildDisposition::Close),
            mk(ChildDisposition::Close),
            mk(ChildDisposition::Park),
            mk(ChildDisposition::RetainedAlreadyTerminal),
            mk(ChildDisposition::RetainedOtherParent),
            mk(ChildDisposition::RetainedExternalDependent),
        ];
        let counts = tally(&findings);
        assert_eq!(
            counts,
            DispositionCounts {
                close: 2,
                park: 1,
                retained_already_terminal: 1,
                retained_other_parent: 1,
                retained_external_dependent: 1,
            }
        );
    }

    // ── Repository integration tests ───────────────────────────────────────
    //
    // These exercise the read-only classification against a real in-memory
    // Postgres database, covering each guard outcome without mutating rows.

    use djinn_core::events::EventBus;

    fn make_repo(db: &Database, events: &EventBus) -> TaskRepository {
        TaskRepository::new(db.clone(), events.clone())
    }

    async fn make_project(db: &Database) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        // Runtime query (not the compile-time `query!` macro) so it does not
        // need an entry in the `.sqlx` offline cache.
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind("pd-project")
        .bind("test")
        .bind("pd-project")
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn make_epic(db: &Database, project_id: &str, short_id: &str) -> String {
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '[]'::jsonb)",
        )
        .bind(&epic_id)
        .bind(project_id)
        .bind(short_id)
        .bind("Epic")
        .bind("")
        .bind("")
        .bind("")
        .bind("")
        .execute(db.pool())
        .await
        .unwrap();
        epic_id
    }

    async fn make_task(db: &Database, epic_id: &str, status: &str, short_id: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let project_id = epic_project(db, epic_id).await;
        let title = format!("Task {short_id}");
        sqlx::query(
            r#"INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                    issue_type, status, priority, owner, labels, acceptance_criteria, memory_refs)
               VALUES ($1, $2, $3, $4, $5, '', '', 'task', $6, 1, '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)"#,
        )
        .bind(&id)
        .bind(&project_id)
        .bind(short_id)
        .bind(epic_id)
        .bind(&title)
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn epic_project(db: &Database, epic_id: &str) -> String {
        let row: (String,) = sqlx::query_as("SELECT project_id FROM epics WHERE id = $1")
            .bind(epic_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        row.0
    }

    async fn add_blocker(db: &Database, task_id: &str, blocking_task_id: &str) {
        sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
            .bind(task_id)
            .bind(blocking_task_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn make_proposal(db: &Database, short_id: &str, status: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let title = format!("Proposal {short_id}");
        sqlx::query(
            r#"INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq)
               VALUES ($1, $2, $3, '', 'markdown', '[]'::jsonb, $4, 1)"#,
        )
        .bind(&id)
        .bind(short_id)
        .bind(&title)
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn link_epic(db: &Database, proposal_id: &str, epic_id: &str, project_id: &str) {
        sqlx::query(
            "INSERT INTO proposal_epics (proposal_id, epic_id, project_id) VALUES ($1, $2, $3)",
        )
        .bind(proposal_id)
        .bind(epic_id)
        .bind(project_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    fn finding_for<'a>(plan: &'a DispositionPlan, task_id: &str) -> &'a DispositionFinding {
        plan.findings
            .iter()
            .find(|f| f.task_id == task_id)
            .unwrap_or_else(|| panic!("no finding for task {task_id}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_ready_child_classifies_as_close() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        // open → close-ready
        let t_open = make_task(&db, &epic, "open", "t1").await;
        // needs_lead_intervention → close-ready
        let t_nli = make_task(&db, &epic, "needs_lead_intervention", "t2").await;

        let scope = DispositionScope::for_epic_close(&epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan.counts.close, 2);
        assert_eq!(
            finding_for(&plan, &t_open).disposition,
            ChildDisposition::Close
        );
        assert_eq!(
            finding_for(&plan, &t_nli).disposition,
            ChildDisposition::Close
        );
        assert!(
            plan.findings
                .iter()
                .all(|f| f.guard_reason.is_none() || f.disposition.retained())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_flight_child_classifies_as_park() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let t_ip = make_task(&db, &epic, "in_progress", "t1").await;
        let t_pr = make_task(&db, &epic, "pr_review", "t2").await;

        let scope = DispositionScope::for_epic_close(&epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan.counts.park, 2);
        assert_eq!(
            finding_for(&plan, &t_ip).disposition,
            ChildDisposition::Park
        );
        assert_eq!(
            finding_for(&plan, &t_pr).disposition,
            ChildDisposition::Park
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn already_closed_child_is_retained_terminal() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let t_closed = make_task(&db, &epic, "closed", "t1").await;

        let scope = DispositionScope::for_epic_close(&epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan.counts.retained_already_terminal, 1);
        let f = finding_for(&plan, &t_closed);
        assert_eq!(f.disposition, ChildDisposition::RetainedAlreadyTerminal);
        assert_eq!(f.guard_reason, Some(GuardReason::AlreadyTerminal));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn other_open_proposal_parent_is_retained() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        // A building (open) proposal graduated this epic.
        let proposal = make_proposal(&db, "p1", "building").await;
        link_epic(&db, &proposal, &epic, &project).await;

        let t = make_task(&db, &epic, "open", "t1").await;

        let scope = DispositionScope::for_epic_close(&epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan.counts.retained_other_parent, 1);
        let f = finding_for(&plan, &t);
        assert_eq!(f.disposition, ChildDisposition::RetainedOtherParent);
        match &f.guard_reason {
            Some(GuardReason::OtherOpenProposalParent { open_proposals }) => {
                assert_eq!(open_proposals.len(), 1);
                assert_eq!(open_proposals[0].proposal_id, proposal);
                assert_eq!(open_proposals[0].status, "building");
            }
            other => panic!("expected OtherOpenProposalParent, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_proposal_does_not_retain_child() {
        // A done/archived proposal is NOT an open parent, so the child should
        // classify normally (close-ready here).
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let proposal = make_proposal(&db, "p1", "done").await;
        link_epic(&db, &proposal, &epic, &project).await;

        let t = make_task(&db, &epic, "open", "t1").await;

        let scope = DispositionScope::for_epic_close(&epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan.counts.close, 1);
        assert_eq!(finding_for(&plan, &t).disposition, ChildDisposition::Close);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_open_dependent_is_retained() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let closing_epic = make_epic(&db, &project, "e1").await;
        let other_epic = make_epic(&db, &project, "e2").await;

        // Child in the closing epic.
        let child = make_task(&db, &closing_epic, "open", "t1").await;
        // External open dependent in a DIFFERENT epic, blocked by the child.
        let dependent = make_task(&db, &other_epic, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let scope = DispositionScope::for_epic_close(&closing_epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan.counts.retained_external_dependent, 1);
        let f = finding_for(&plan, &child);
        assert_eq!(f.disposition, ChildDisposition::RetainedExternalDependent);
        match &f.guard_reason {
            Some(GuardReason::ExternalOpenDependent { dependents }) => {
                assert_eq!(dependents.len(), 1);
                assert_eq!(dependents[0].task_id, dependent);
                assert_eq!(dependents[0].status, "open");
            }
            other => panic!("expected ExternalOpenDependent, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn internal_only_dependent_does_not_block_disposition() {
        // A dependent that lives INSIDE the same closing epic is internal and
        // must not prevent disposition of the child.
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let closing_epic = make_epic(&db, &project, "e1").await;

        let child = make_task(&db, &closing_epic, "open", "t1").await;
        // Dependent in the SAME closing epic.
        let dependent = make_task(&db, &closing_epic, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let scope = DispositionScope::for_epic_close(&closing_epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        // Both children should classify (the child as close, the dependent as
        // close too). Neither is retained on an external-dependent guard.
        assert_eq!(plan.counts.close, 2);
        assert_eq!(plan.counts.retained_external_dependent, 0);
        for f in &plan.findings {
            assert_ne!(
                f.disposition,
                ChildDisposition::RetainedExternalDependent,
                "internal dependent must not retain child"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_abort_apply_excludes_scoped_proposal_but_retains_other_parent() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let aborting = make_proposal(&db, "p1", "building").await;
        let other = make_proposal(&db, "p2", "building").await;
        link_epic(&db, &aborting, &epic, &project).await;
        link_epic(&db, &other, &epic, &project).await;
        let child = make_task(&db, &epic, "open", "t1").await;

        let scope = DispositionScope::for_proposal_abort(&aborting, vec![epic.clone()]);
        let mut tx = db.pool().begin().await.unwrap();
        let plan = apply_parent_disposition_tx(&mut tx, &scope).await.unwrap();
        tx.commit().await.unwrap();

        let finding = finding_for(&plan, &child);
        assert_eq!(finding.disposition, ChildDisposition::RetainedOtherParent);
        match &finding.guard_reason {
            Some(GuardReason::OtherOpenProposalParent { open_proposals }) => {
                assert_eq!(
                    open_proposals,
                    &vec![OpenProposalRef {
                        proposal_id: other,
                        status: "building".to_owned()
                    }]
                );
            }
            other => panic!("expected other-open-parent evidence, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_abort_apply_cascades_through_internal_multi_epic_dependents() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let first_epic = make_epic(&db, &project, "e1").await;
        let second_epic = make_epic(&db, &project, "e2").await;
        let proposal = make_proposal(&db, "p1", "building").await;
        link_epic(&db, &proposal, &first_epic, &project).await;
        link_epic(&db, &proposal, &second_epic, &project).await;
        let child = make_task(&db, &first_epic, "open", "t1").await;
        let dependent = make_task(&db, &second_epic, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let scope = DispositionScope::for_proposal_abort(
            &proposal,
            vec![first_epic.clone(), second_epic.clone()],
        );
        let mut tx = db.pool().begin().await.unwrap();
        let plan = apply_parent_disposition_tx(&mut tx, &scope).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(plan.counts.close, 2);
        assert_eq!(
            finding_for(&plan, &child).disposition,
            ChildDisposition::Close
        );
        assert_eq!(
            finding_for(&plan, &dependent).disposition,
            ChildDisposition::Close
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn obsolete_epic_reconcile_apply_retains_external_dependent_with_evidence() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let obsolete_epic = make_epic(&db, &project, "e1").await;
        let external_epic = make_epic(&db, &project, "e2").await;
        let proposal = make_proposal(&db, "p1", "building").await;
        link_epic(&db, &proposal, &obsolete_epic, &project).await;
        let child = make_task(&db, &obsolete_epic, "open", "t1").await;
        let dependent = make_task(&db, &external_epic, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let scope =
            DispositionScope::for_proposal_obsolete_epic_reconcile(&proposal, &obsolete_epic);
        let mut tx = db.pool().begin().await.unwrap();
        let plan = apply_parent_disposition_tx(&mut tx, &scope).await.unwrap();
        tx.commit().await.unwrap();

        let finding = finding_for(&plan, &child);
        assert_eq!(
            finding.disposition,
            ChildDisposition::RetainedExternalDependent
        );
        match &finding.guard_reason {
            Some(GuardReason::ExternalOpenDependent { dependents }) => {
                assert_eq!(dependents.len(), 1);
                assert_eq!(dependents[0].task_id, dependent);
                assert_eq!(dependents[0].status, "open");
            }
            other => panic!("expected external-dependent evidence, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_order_already_terminal_before_other_guards() {
        // An already-closed child that also has an external dependent should
        // be retained-already-terminal (guard 1 wins).
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let closing_epic = make_epic(&db, &project, "e1").await;
        let other_epic = make_epic(&db, &project, "e2").await;

        let child = make_task(&db, &closing_epic, "closed", "t1").await;
        let dependent = make_task(&db, &other_epic, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let scope = DispositionScope::for_epic_close(&closing_epic);
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();

        let f = finding_for(&plan, &child);
        assert_eq!(f.disposition, ChildDisposition::RetainedAlreadyTerminal);
        assert_eq!(f.guard_reason, Some(GuardReason::AlreadyTerminal));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_scope_returns_empty_plan() {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let scope = DispositionScope {
            entry_point: DispositionEntryPoint::EpicClose,
            epic_ids: vec![],
            proposal_id: None,
        };
        let plan = repo.classify_parent_disposition(&scope).await.unwrap();
        assert!(plan.findings.is_empty());
        assert_eq!(plan.counts, DispositionCounts::default());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plan_does_not_mutate_rows() {
        // Classification is read-only: running it twice must yield identical
        // plans and leave all task statuses unchanged.
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let repo = make_repo(&db, &events);
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let t1 = make_task(&db, &epic, "open", "t1").await;
        let t2 = make_task(&db, &epic, "in_progress", "t2").await;

        let scope = DispositionScope::for_epic_close(&epic);
        let plan1 = repo.classify_parent_disposition(&scope).await.unwrap();
        let plan2 = repo.classify_parent_disposition(&scope).await.unwrap();

        assert_eq!(plan1.counts, plan2.counts);
        assert_eq!(plan1.findings.len(), plan2.findings.len());

        // Statuses must be unchanged (no mutation).
        let s1: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&t1)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let s2: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&t2)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(s1.0, "open");
        assert_eq!(s2.0, "in_progress");
    }

    // ── Doctor repair tests ───────────────────────────────────────────────
    //
    // These tests prove that `apply_doctor_repair_tx` safely applies the
    // mutating repair, skips guarded/stale rows, and is idempotent.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_closes_ready_orphan() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "open", "t1").await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::Closed {
                task_id: child.clone(),
                from_status: "open".to_owned(),
            }
        );

        let row: (String, String) =
            sqlx::query_as("SELECT status, close_reason FROM tasks WHERE id = $1")
                .bind(&child)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "closed");
        assert_eq!(row.1, "parent_closed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_parks_in_flight_orphan() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "in_progress", "t1").await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "in_progress", "park", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::Parked {
                task_id: child.clone(),
                from_status: "in_progress".to_owned(),
                reason: "historical_parent_closed_in_flight",
            }
        );

        let row: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.0, "needs_lead_intervention");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_parks_pr_active_orphan_with_pr_reason() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "pr_review", "t1").await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "pr_review", "park", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::Parked {
                task_id: child.clone(),
                from_status: "pr_review".to_owned(),
                reason: "historical_parent_closed_pr_active",
            }
        );

        let row: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.0, "needs_lead_intervention");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_skips_already_closed() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "closed", "t1").await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome = apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[], &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::SkippedAlreadyClosed {
                task_id: child.clone(),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_skips_status_drift() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        // Task is currently 'in_progress' but the snapshot said 'open'.
        let child = make_task(&db, &epic, "in_progress", "t1").await;

        let mut tx = db.pool().begin().await.unwrap();
        // Simulate a stale snapshot where status was "open" but is now "in_progress".
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::SkippedStatusDrift {
                task_id: child.clone(),
                snapshot_status: "open".to_owned(),
                current_status: "in_progress".to_owned(),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_skips_retain_finding() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "open", "t1").await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome = apply_doctor_repair_tx(&mut tx, &child, "open", "retain", &[], &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::SkippedRetain {
                task_id: child.clone(),
            }
        );

        // Task must be unchanged.
        let row: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.0, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_skips_other_open_parent() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "open", "t1").await;
        // Link a building proposal to the epic so it becomes an other-open-parent.
        let proposal = make_proposal(&db, "p1", "building").await;
        link_epic(&db, &proposal, &epic, &project).await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::SkippedOtherOpenParent {
                task_id: child.clone(),
            }
        );

        // Task must be unchanged.
        let row: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.0, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_skips_external_open_dependent() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let closing_epic = make_epic(&db, &project, "e1").await;
        let other_epic = make_epic(&db, &project, "e2").await;
        let child = make_task(&db, &closing_epic, "open", "t1").await;
        let dependent = make_task(&db, &other_epic, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome = apply_doctor_repair_tx(
            &mut tx,
            &child,
            "open",
            "close",
            &[closing_epic.clone()],
            &[],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::SkippedExternalDependent {
                task_id: child.clone(),
            }
        );

        // Task must be unchanged.
        let row: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(row.0, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_skips_not_found() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        let outcome = apply_doctor_repair_tx(&mut tx, "nonexistent-id", "open", "close", &[], &[])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::SkippedNotFound {
                task_id: "nonexistent-id".to_owned(),
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_emits_audit_evidence() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "open", "t1").await;

        let before_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM activity_log WHERE task_id = $1")
                .bind(&child)
                .fetch_one(db.pool())
                .await
                .unwrap();

        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert!(outcome.applied());

        let after_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM activity_log WHERE task_id = $1")
                .bind(&child)
                .fetch_one(db.pool())
                .await
                .unwrap();

        // Should have at least 2 new entries: status_changed + doctor_fix_repair.
        assert_eq!(after_count.0 - before_count.0, 2);

        // Verify the doctor_fix_repair audit evidence.
        let audit: serde_json::Value = sqlx::query_scalar(
            "SELECT payload FROM activity_log WHERE task_id = $1 AND event_type = 'doctor_fix_repair'",
        )
        .bind(&child)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let audit: serde_json::Value = serde_json::from_value(audit).unwrap();
        assert_eq!(audit["source"], "doctor_fix");
        assert_eq!(audit["check"], "closed_parent_open_children");
        assert_eq!(audit["original_parent_ids"][0], epic);
        assert_eq!(audit["from_status"], "open");
        assert_eq!(audit["to_status"], "closed");
        assert_eq!(audit["reason"], "parent_closed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_preserves_pr_url_when_parking() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "pr_review", "t1").await;
        // Set a PR URL.
        sqlx::query("UPDATE tasks SET pr_url = $1 WHERE id = $2")
            .bind("https://github.com/test/repo/pull/42")
            .bind(&child)
            .execute(db.pool())
            .await
            .unwrap();

        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "pr_review", "park", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert!(outcome.applied());

        // The PR preservation audit entry must exist.
        let pr_audit: Option<String> = sqlx::query_scalar(
            "SELECT payload::text FROM activity_log WHERE task_id = $1 AND event_type = 'doctor_fix_pr_preserved'",
        )
        .bind(&child)
        .fetch_optional(db.pool())
        .await
        .unwrap();
        assert!(pr_audit.is_some(), "PR preservation audit must exist");
        let payload: serde_json::Value = serde_json::from_str(&pr_audit.unwrap()).unwrap();
        assert_eq!(
            payload["preserved_pr_url"],
            "https://github.com/test/repo/pull/42"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "open", "t1").await;

        // First repair: closes the task.
        let mut tx = db.pool().begin().await.unwrap();
        let outcome1 =
            apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert!(outcome1.applied());

        // Second repair with the same snapshot: must skip (already closed).
        let mut tx = db.pool().begin().await.unwrap();
        let outcome2 =
            apply_doctor_repair_tx(&mut tx, &child, "open", "close", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert!(!outcome2.applied());
        assert!(matches!(
            outcome2,
            DoctorRepairOutcome::SkippedAlreadyClosed { .. }
        ));

        // Verify only one set of audit entries was written (not duplicated).
        let repair_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM activity_log WHERE task_id = $1 AND event_type = 'doctor_fix_repair'")
                .bind(&child)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(repair_count.0, 1, "repair audit must not be duplicated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_parked_task_is_not_redispatched_as_orphan() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let epic = make_epic(&db, &project, "e1").await;
        let child = make_task(&db, &epic, "in_progress", "t1").await;

        // Repair parks the in-flight orphan.
        let mut tx = db.pool().begin().await.unwrap();
        let outcome =
            apply_doctor_repair_tx(&mut tx, &child, "in_progress", "park", &[epic.clone()], &[])
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert!(outcome.applied());

        // The task is now needs_lead_intervention, which is a close-ready status.
        // A second repair with the new snapshot would re-classify it.
        // But since it's already in the "close-ready" bucket, it would close.
        // The key idempotency check: the task is no longer dispatchable as an
        // in-progress orphan.
        let row: (String,) = sqlx::query_as("SELECT status FROM tasks WHERE id = $1")
            .bind(&child)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(
            row.0, "needs_lead_intervention",
            "parked task must not be in_progress (dispatchable)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn doctor_repair_uses_proposal_scope_for_internal_dependents() {
        let db = Database::open_in_memory().unwrap();
        let project = make_project(&db).await;
        let e1 = make_epic(&db, &project, "e1").await;
        let e2 = make_epic(&db, &project, "e2").await;
        let proposal = make_proposal(&db, "p1", "rejected").await;
        link_epic(&db, &proposal, &e1, &project).await;
        link_epic(&db, &proposal, &e2, &project).await;

        let child = make_task(&db, &e1, "open", "t1").await;
        let dependent = make_task(&db, &e2, "open", "t2").await;
        add_blocker(&db, &dependent, &child).await;

        let mut tx = db.pool().begin().await.unwrap();
        let outcome = apply_doctor_repair_tx(
            &mut tx,
            &child,
            "open",
            "close",
            &[e1.clone()],
            &[proposal.clone()],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            outcome,
            DoctorRepairOutcome::Closed {
                task_id: child.clone(),
                from_status: "open".to_owned(),
            }
        );

        let row: (String, String) =
            sqlx::query_as("SELECT status, close_reason FROM tasks WHERE id = $1")
                .bind(&child)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "closed");
        assert_eq!(row.1, "parent_closed");
    }
}
