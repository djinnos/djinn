// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::HashMap;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    Proposal, ProposalFeedback, ProposalRevision, ProposalSignoff, ProposalTarget,
};

use crate::database::Database;
use crate::{Error, Result};

// Global proposals layer (Phase 0). A `proposal` is project-independent; it
// targets projects via `proposal_targets` (editable M:N) and carries unified
// discussion+suggestion `proposal_feedback`. This repository mirrors
// `epic.rs` conventions: `query_as!` with inlined SELECT projections, `$N`
// params, and an event emitted after every mutation.

// ── Query / result types ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum SqlParam {
    Text(String),
}

/// Filters and pagination for [`ProposalRepository::list_filtered`].
pub struct ProposalListQuery {
    pub status: Option<String>,
    pub text: Option<String>,
    pub author_user_id: Option<String>,
    /// Restrict to proposals that target this project (UUID).
    pub target_project_id: Option<String>,
    pub sort: String,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ProposalListQuery {
    fn default() -> Self {
        Self {
            status: None,
            text: None,
            author_user_id: None,
            target_project_id: None,
            sort: "created_desc".to_owned(),
            limit: 25,
            offset: 0,
        }
    }
}

pub struct ProposalListResult {
    /// Each proposal paired with its unresolved-feedback count (drives the
    /// per-row badge in the proposals list).
    pub proposals: Vec<(Proposal, i64)>,
    pub total_count: i64,
}

/// List-only row: the `Proposal` columns plus the correlated unresolved-feedback
/// count. Kept separate from `Proposal` (which maps 1:1 to columns via the
/// `query_as!` macro paths) so the list's extra aggregate doesn't leak into
/// every get/resolve projection.
#[derive(sqlx::FromRow)]
struct ProposalListRow {
    #[sqlx(flatten)]
    proposal: Proposal,
    unresolved_feedback_count: i64,
}

pub struct ProposalCreateInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// JSON array string of acceptance-criteria; `None` defaults to `[]`.
    pub acceptance_criteria: Option<&'a str>,
    /// Initial status; `None` defaults to `draft`.
    pub status: Option<&'a str>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<&'a str>,
}

pub struct ProposalUpdateInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// JSON array string of acceptance-criteria.
    pub acceptance_criteria: &'a str,
    pub status: &'a str,
    pub superseded_by: Option<&'a str>,
    /// Body encoding: `markdown` (default) or `mdx`.
    pub body_format: Option<&'a str>,
}

pub struct ProposalFeedbackCreateInput<'a> {
    pub proposal_id: &'a str,
    pub parent_id: Option<&'a str>,
    /// `user` (default) or `ai`.
    pub author_kind: &'a str,
    pub author_model: Option<&'a str>,
    pub body: &'a str,
    pub target_section: Option<&'a str>,
}

/// A Planner-authored acceptance-criteria spec amendment. Unlike
/// [`ProposalRepository::set_acceptance_criteria`], these operations are real
/// spec edits: they bump the proposal revision and write an audit trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalAcceptanceCriteriaAmendment<'a> {
    Rewrite { index: usize, criterion: &'a str },
    Drop { index: usize },
    Waive { index: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct ProposalAcceptanceCriteriaAuditEntry {
    operation: &'static str,
    index: usize,
    old_criterion: serde_json::Value,
    new_criterion: serde_json::Value,
}

struct ProposalStatusEvent<'a> {
    proposal_id: &'a str,
    seq: i32,
    title: &'a str,
    body: &'a str,
    body_format: &'a str,
    acceptance_criteria: &'a serde_json::Value,
    edited_by: Option<&'a str>,
    status_from: &'a str,
    status_to: &'a str,
}

pub struct ProposalRepository {
    db: Database,
    events: EventBus,
}

impl ProposalRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub async fn get(&self, id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id
             FROM proposals WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id
             FROM proposals WHERE short_id = $1"#,
            short_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a proposal by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id
             FROM proposals WHERE id = $1 OR short_id = $2"#,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn create(&self, input: ProposalCreateInput<'_>) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let status = input.status.unwrap_or("draft");
        let body_format = input.body_format.unwrap_or("markdown");
        let ac_str = input.acceptance_criteria.unwrap_or("[]");
        let acceptance_criteria: serde_json::Value = serde_json::from_str(ac_str).map_err(|e| {
            Error::InvalidData(format!(
                "invalid json for proposals.acceptance_criteria: {e}"
            ))
        })?;
        // Author is the authenticated MCP caller, mirroring how epics stamp
        // `created_by_user_id`. `None` when no user context is in scope.
        let author_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, author_user_id, latest_revision_seq)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1)",
            id,
            short_id,
            input.title,
            input.body,
            body_format,
            acceptance_criteria,
            status,
            author_user_id
        )
        .execute(self.db.pool())
        .await?;
        // Seed revision 1 with the initial spec so every proposal has a head to
        // diff against.
        self.insert_revision(
            &id,
            1,
            input.title,
            input.body,
            body_format,
            &acceptance_criteria,
            author_user_id.as_deref(),
        )
        .await?;
        let proposal = self.get_required(&id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_created(&proposal));
        Ok(proposal)
    }

    pub async fn update(&self, id: &str, input: ProposalUpdateInput<'_>) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let acceptance_criteria: serde_json::Value =
            serde_json::from_str(input.acceptance_criteria).map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json for proposals.acceptance_criteria: {e}"
                ))
            })?;
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {id}")))?;
        let body_format = input.body_format.unwrap_or(&current.body_format);
        let current_ac: serde_json::Value =
            serde_json::from_str(&current.acceptance_criteria).unwrap_or(serde_json::json!([]));
        // A "material" edit changes the spec (title/body/AC), not just status.
        // Only material edits append a revision and disturb sign-offs.
        let content_changed = input.title != current.title
            || input.body != current.body
            || body_format != current.body_format
            || acceptance_criteria != current_ac;

        // Stale/hard rule: editing the spec of an *approved* proposal reverts it
        // to in_review and clears its sign-offs (you changed an approved spec).
        // While in_review, edits leave sign-offs in place — they go stale
        // automatically because the head revision advances past them.
        let demote = content_changed && current.status == "approved";
        let building_amend = content_changed && current.status == "building";
        let effective_status = if building_amend {
            "building"
        } else if demote && input.status == "approved" {
            "in_review"
        } else {
            input.status
        };
        let next_seq = if content_changed {
            current.latest_revision_seq + 1
        } else {
            current.latest_revision_seq
        };
        let status_changed = current.status != effective_status;
        let record_done_status_event =
            !content_changed && status_changed && effective_status == "done";

        sqlx::query!(
            r#"UPDATE proposals SET title = $1, body = $2, body_format = $10, acceptance_criteria = $3, status = $4,
                    superseded_by = $5, latest_revision_seq = $8,
                    pending_reconcile = CASE WHEN $9 THEN true ELSE pending_reconcile END,
                    closed_at = CASE WHEN $6 IN ('done', 'rejected', 'archived', 'superseded')
                        THEN COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                        ELSE NULL END,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $7"#,
            input.title,
            input.body,
            acceptance_criteria,
            effective_status,
            input.superseded_by,
            effective_status,
            id,
            next_seq,
            building_amend,
            body_format,
        )
        .execute(self.db.pool())
        .await?;

        if content_changed {
            let editor = djinn_core::auth_context::current_user_id();
            self.insert_revision(
                id,
                next_seq,
                input.title,
                input.body,
                body_format,
                &acceptance_criteria,
                editor.as_deref(),
            )
            .await?;
        } else if record_done_status_event {
            let editor = djinn_core::auth_context::current_user_id();
            self.insert_status_event(ProposalStatusEvent {
                proposal_id: id,
                seq: next_seq,
                title: input.title,
                body: input.body,
                body_format,
                acceptance_criteria: &acceptance_criteria,
                edited_by: editor.as_deref(),
                status_from: &current.status,
                status_to: effective_status,
            })
            .await?;
        }
        if demote {
            sqlx::query!("DELETE FROM proposal_signoffs WHERE proposal_id = $1", id)
                .execute(self.db.pool())
                .await?;
        }

        // Re-evaluate the approval gate after any status/spec change. Sign-offs
        // can already be present when a proposal *enters* in_review (e.g. signed
        // while in draft, or promoted via the status dropdown); add_signoff only
        // reconciles at sign-off time, so without this the gate would never fire.
        if current.status != "building" {
            self.reconcile_approval(id).await?;
        }
        let proposal = self.get_required(id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("DELETE FROM proposals WHERE id = $1", id)
            .execute(self.db.pool())
            .await?;
        self.events.send(DjinnEventEnvelope::proposal_deleted(id));
        Ok(())
    }

    // ── Targets (editable M:N to projects) ───────────────────────────────────

    pub async fn targets(&self, proposal_id: &str) -> Result<Vec<ProposalTarget>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalTarget,
            r#"SELECT proposal_id, project_id, role, created_at
             FROM proposal_targets WHERE proposal_id = $1 ORDER BY created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Add (or re-role) a target project. Idempotent on `(proposal_id,
    /// project_id)`; re-adding updates the role. The `project_id` FK must
    /// reference a registered project.
    pub async fn add_target(&self, proposal_id: &str, project_id: &str, role: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, $3)
             ON CONFLICT (proposal_id, project_id) DO UPDATE SET role = EXCLUDED.role",
            proposal_id,
            project_id,
            role
        )
        .execute(self.db.pool())
        .await?;
        if let Some(proposal) = self.get(proposal_id).await? {
            self.events
                .send(DjinnEventEnvelope::proposal_updated(&proposal));
        }
        Ok(())
    }

    /// Remove a target project. No-op if absent.
    pub async fn remove_target(&self, proposal_id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM proposal_targets WHERE proposal_id = $1 AND project_id = $2",
            proposal_id,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        if let Some(proposal) = self.get(proposal_id).await? {
            self.events
                .send(DjinnEventEnvelope::proposal_updated(&proposal));
        }
        Ok(())
    }

    // ── Feedback (discussion; resolved through djinn, not applied directly) ──

    pub async fn feedback(&self, proposal_id: &str) -> Result<Vec<ProposalFeedback>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalFeedback,
            r#"SELECT id, proposal_id, parent_id, author_kind, author_user_id, author_model,
                    body, target_section, resolved_at, resolved_revision_seq, resolved_by_user_id, created_at, updated_at
             FROM proposal_feedback WHERE proposal_id = $1 ORDER BY created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get_feedback(&self, feedback_id: &str) -> Result<Option<ProposalFeedback>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalFeedback,
            r#"SELECT id, proposal_id, parent_id, author_kind, author_user_id, author_model,
                    body, target_section, resolved_at, resolved_revision_seq, resolved_by_user_id, created_at, updated_at
             FROM proposal_feedback WHERE id = $1"#,
            feedback_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn add_feedback(
        &self,
        input: ProposalFeedbackCreateInput<'_>,
    ) -> Result<ProposalFeedback> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let author_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO proposal_feedback
                (id, proposal_id, parent_id, author_kind, author_user_id, author_model, body, target_section)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            id,
            input.proposal_id,
            input.parent_id,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.body,
            input.target_section
        )
        .execute(self.db.pool())
        .await?;
        let feedback = self.get_feedback_required(&id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_feedback_created(
                input.proposal_id,
                &feedback,
            ));
        Ok(feedback)
    }

    /// Resolve a feedback entry: collapse it out of the active thread. Pass the
    /// revision that addressed it (when djinn applied a spec change) or `None`
    /// for a plain dismissal. Stamps the resolving user via `current_user_id()`.
    /// Idempotent — re-resolving just refreshes the resolution.
    pub async fn set_feedback_resolved(
        &self,
        feedback_id: &str,
        resolved_revision_seq: Option<i32>,
    ) -> Result<ProposalFeedback> {
        self.db.ensure_initialized().await?;
        let resolved_by = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            r#"UPDATE proposal_feedback SET
                    resolved_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    resolved_revision_seq = $1,
                    resolved_by_user_id = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
            resolved_revision_seq,
            resolved_by,
            feedback_id
        )
        .execute(self.db.pool())
        .await?;
        let feedback = self.get_feedback_required(feedback_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_feedback_created(
                &feedback.proposal_id,
                &feedback,
            ));
        Ok(feedback)
    }

    // ── Listing ──────────────────────────────────────────────────────────────

    pub async fn list_filtered(&self, query: ProposalListQuery) -> Result<ProposalListResult> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = proposal_build_where(
            &query.status,
            &query.text,
            &query.author_user_id,
            &query.target_project_id,
        );
        let order_sql = proposal_sort_to_sql(&query.sort);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM proposals WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            total_q = total_q.bind(s.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let limit_ph = format!("${}", params.len() + 1);
        let offset_ph = format!("${}", params.len() + 2);
        // NOTE: dynamic SQL (WHERE + ORDER built from optional filters) — compile-time check not possible.
        // The correlated subquery counts unresolved feedback per row (cheap via
        // the `proposal_feedback_unresolved` partial index) for the list badge.
        let sql = format!(
            r#"SELECT id, short_id, title, body, body_format, acceptance_criteria::text AS acceptance_criteria,
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id,
                    (SELECT COUNT(*) FROM proposal_feedback pf
                       WHERE pf.proposal_id = proposals.id AND pf.resolved_at IS NULL) AS unresolved_feedback_count
             FROM proposals WHERE {where_sql} ORDER BY {order_sql} LIMIT {limit_ph} OFFSET {offset_ph}"#
        );
        let mut q = sqlx::query_as::<_, ProposalListRow>(&sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            q = q.bind(s.clone());
        }
        let proposals = q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?
            .into_iter()
            .map(|row| (row.proposal, row.unresolved_feedback_count))
            .collect();

        Ok(ProposalListResult {
            proposals,
            total_count: total,
        })
    }

    // ── Revisions + sign-offs ────────────────────────────────────────────────

    async fn insert_revision(
        &self,
        proposal_id: &str,
        seq: i32,
        title: &str,
        body: &str,
        body_format: &str,
        acceptance_criteria: &serde_json::Value,
        edited_by: Option<&str>,
    ) -> Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO proposal_revisions
                (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'spec_revision')"#,
        )
        .bind(id)
        .bind(proposal_id)
        .bind(seq)
        .bind(title)
        .bind(body)
        .bind(body_format)
        .bind(acceptance_criteria)
        .bind(edited_by)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    async fn insert_status_event(&self, event: ProposalStatusEvent<'_>) -> Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO proposal_revisions
                (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id,
                 event_kind, status_from, status_to)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'status_change', $9, $10)",
        )
        .bind(id)
        .bind(event.proposal_id)
        .bind(event.seq)
        .bind(event.title)
        .bind(event.body)
        .bind(event.body_format)
        .bind(event.acceptance_criteria)
        .bind(event.edited_by)
        .bind(event.status_from)
        .bind(event.status_to)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Revisions/history events of a proposal, oldest first.
    pub async fn revisions(&self, proposal_id: &str) -> Result<Vec<ProposalRevision>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalRevision>(
            r#"SELECT id, proposal_id, seq, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    edited_by_user_id, event_kind, status_from, status_to,
                    event_metadata::text AS event_metadata, created_at
             FROM proposal_revisions
             WHERE proposal_id = $1
             ORDER BY created_at, id"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn signoffs(&self, proposal_id: &str) -> Result<Vec<ProposalSignoff>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalSignoff,
            r#"SELECT proposal_id, kind, user_id, revision_seq, created_at
             FROM proposal_signoffs WHERE proposal_id = $1 ORDER BY created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Record (or refresh) a `kind` sign-off by `user_id`, anchored to the head
    /// revision. Idempotent per (proposal, kind, user). Reconciles approval.
    pub async fn add_signoff(
        &self,
        proposal_id: &str,
        kind: &str,
        user_id: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        sqlx::query!(
            r#"INSERT INTO proposal_signoffs (proposal_id, kind, user_id, revision_seq)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (proposal_id, kind, user_id) DO UPDATE
                 SET revision_seq = EXCLUDED.revision_seq,
                     created_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
            proposal_id,
            kind,
            user_id,
            proposal.latest_revision_seq
        )
        .execute(self.db.pool())
        .await?;
        if proposal.status != "building" {
            self.reconcile_approval(proposal_id).await?;
        }
        let updated = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&updated));
        Ok(updated)
    }

    /// Withdraw `user_id`'s `kind` sign-off. Reconciles approval (may demote
    /// `approved → in_review` if the gate is no longer met).
    pub async fn clear_signoff(
        &self,
        proposal_id: &str,
        kind: &str,
        user_id: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM proposal_signoffs WHERE proposal_id = $1 AND kind = $2 AND user_id = $3",
            proposal_id,
            kind,
            user_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        if proposal.status != "building" {
            self.reconcile_approval(proposal_id).await?;
        }
        let updated = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&updated));
        Ok(updated)
    }

    /// Drive the status off the sign-off state. A `draft` auto-advances to
    /// `in_review` on its first fresh sign-off (the act of signing *is* the
    /// request for review), and any state reaches `approved` once both a scoped
    /// and a technical sign-off are fresh at the head revision. An `approved`
    /// proposal auto-demotes back to `in_review` when that's no longer true.
    async fn reconcile_approval(&self, proposal_id: &str) -> Result<()> {
        let proposal = match self.get(proposal_id).await? {
            Some(p) => p,
            None => return Ok(()),
        };
        let fresh: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT kind) AS "n!: i64" FROM proposal_signoffs
             WHERE proposal_id = $1 AND revision_seq = $2 AND kind IN ('scoped', 'technical')"#,
            proposal_id,
            proposal.latest_revision_seq
        )
        .fetch_one(self.db.pool())
        .await?;
        let both = fresh == 2;
        let any = fresh >= 1;
        let new_status = match proposal.status.as_str() {
            "draft" if both => Some("approved"),
            "draft" if any => Some("in_review"),
            "in_review" if both => Some("approved"),
            "approved" if !both => Some("in_review"),
            "building" => None,
            _ => None,
        };
        if let Some(status) = new_status {
            sqlx::query!(
                r#"UPDATE proposals SET status = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                 WHERE id = $2"#,
                status,
                proposal_id
            )
            .execute(self.db.pool())
            .await?;
        }
        Ok(())
    }

    // ── Graduation ───────────────────────────────────────────────────────────

    /// Distinct participants accountable for the proposal: its author plus
    /// everyone who has signed off. The build owner must be one of these.
    pub async fn participants(&self, proposal_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let mut ids: Vec<String> = sqlx::query_scalar!(
            "SELECT DISTINCT user_id FROM proposal_signoffs WHERE proposal_id = $1",
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?;
        if let Some(p) = self.get(proposal_id).await?
            && let Some(author) = p.author_user_id
            && !ids.contains(&author)
        {
            ids.push(author);
        }
        Ok(ids)
    }

    /// Link a graduated epic to the proposal. Idempotent.
    pub async fn link_epic(
        &self,
        proposal_id: &str,
        epic_id: &str,
        project_id: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "INSERT INTO proposal_epics (proposal_id, epic_id, project_id) VALUES ($1, $2, $3)
             ON CONFLICT (proposal_id, epic_id) DO NOTHING",
            proposal_id,
            epic_id,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        if let Some(proposal) = self.get(proposal_id).await?
            && proposal.status == "building"
        {
            let seq = proposal
                .last_reconciled_revision_seq
                .unwrap_or(proposal.latest_revision_seq);
            self.record_epic_reconciliation(proposal_id, epic_id, seq)
                .await?;
        }
        Ok(())
    }

    /// `(epic_id, project_id)` pairs this proposal graduated into.
    pub async fn graduated_epics(&self, proposal_id: &str) -> Result<Vec<(String, String)>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            "SELECT epic_id, project_id FROM proposal_epics WHERE proposal_id = $1 ORDER BY created_at",
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.epic_id, r.project_id))
            .collect())
    }

    /// Stamp that one graduated epic has been reconciled against a proposal
    /// revision. Idempotent for repeated reconcile runs of the same revision.
    pub async fn record_epic_reconciliation(
        &self,
        proposal_id: &str,
        epic_id: &str,
        revision_seq: i32,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO proposal_reconciliations (proposal_id, epic_id, revision_seq)
               VALUES ($1, $2, $3)
               ON CONFLICT (proposal_id, epic_id, revision_seq) DO NOTHING"#,
        )
        .bind(proposal_id)
        .bind(epic_id)
        .bind(revision_seq)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Latest reconciled proposal revision per graduated epic for a proposal.
    pub async fn latest_epic_reconciliations(
        &self,
        proposal_id: &str,
    ) -> Result<HashMap<String, i32>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, (String, Option<i32>)>(
            r#"SELECT pe.epic_id, MAX(pr.revision_seq) AS revision_seq
               FROM proposal_epics pe
               LEFT JOIN proposal_reconciliations pr
                 ON pr.proposal_id = pe.proposal_id
                AND pr.epic_id = pe.epic_id
               WHERE pe.proposal_id = $1
               GROUP BY pe.epic_id"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(epic_id, revision_seq)| revision_seq.map(|seq| (epic_id, seq)))
            .collect())
    }

    /// Drop every graduated-epic link for a proposal. The missing counterpart
    /// to [`Self::link_epic`] (which only ever inserts): an aborted build must
    /// unlink its epics so a later re-graduation starts from a clean set
    /// instead of accumulating closed epics from prior generations.
    pub async fn unlink_epics(&self, proposal_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM proposal_epics WHERE proposal_id = $1",
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Drop one graduated-epic link for a proposal. Idempotent.
    ///
    /// This is the scoped counterpart to [`Self::unlink_epics`], used by
    /// proposal reconcile when retiring one obsolete epic subtree while leaving
    /// unrelated graduated epics attached to the still-building proposal.
    pub async fn unlink_epic(&self, proposal_id: &str, epic_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM proposal_epics WHERE proposal_id = $1 AND epic_id = $2")
            .bind(proposal_id)
            .bind(epic_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Record the `epic_breakdown` task created at graduation.
    pub async fn set_breakdown_task(&self, proposal_id: &str, task_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE proposals SET build_breakdown_task_id = $1 WHERE id = $2",
            task_id,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Freeze or un-freeze a build. Frozen builds stay `building` but their
    /// epics' tasks are held out of dispatch (see `build_ready_where`).
    pub async fn set_frozen(&self, proposal_id: &str, frozen: bool) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET build_frozen = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            frozen,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Explicit inverse of [`Self::set_building`]: revert an aborted build back
    /// to `approved` so it is immediately re-graduate-able. Clears the build
    /// owner, the breakdown-task link, and any freeze. (Epics are unlinked
    /// separately via [`Self::unlink_epics`].)
    pub async fn revert_to_approved(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET status = 'approved', build_owner_user_id = NULL,
                    build_breakdown_task_id = NULL, build_frozen = false,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Mark a proposal as building, recording the build owner.
    pub async fn set_building(&self, proposal_id: &str, owner_user_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET status = 'building', build_owner_user_id = $1,
                    last_reconciled_revision_seq = latest_revision_seq,
                    pending_reconcile = false,
                    reconciled_at = now(),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            owner_user_id,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Mark the current build as reconciled to the proposal's head revision and
    /// stamp each graduated epic. This is the successful reconcile write site;
    /// callers that apply a reconcile should use this instead of updating
    /// `last_reconciled_revision_seq` directly so per-epic badges stay in sync.
    pub async fn mark_reconciled(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let proposal = self.get_required(proposal_id).await?;
        let revision_seq = proposal.latest_revision_seq;
        let epics = self.graduated_epics(proposal_id).await?;
        for (epic_id, _) in epics {
            self.record_epic_reconciliation(proposal_id, &epic_id, revision_seq)
                .await?;
        }
        sqlx::query(
            r#"UPDATE proposals SET last_reconciled_revision_seq = $1,
                    pending_reconcile = false,
                    reconciled_at = now(),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $2"#,
        )
        .bind(revision_seq)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// The proposal a graduated epic belongs to, if any. Reverse of
    /// [`Self::link_epic`] — used by the coordinator to decide whether closing
    /// an epic completes its parent proposal.
    pub async fn proposal_for_epic(&self, epic_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            "SELECT proposal_id FROM proposal_epics WHERE epic_id = $1 LIMIT 1",
            epic_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some(r) => self.get(&r.proposal_id).await,
            None => Ok(None),
        }
    }

    /// `true` when the proposal has graduated at least one epic AND every
    /// graduated epic is closed. `false` for a proposal with no graduated
    /// epics (nothing has been built yet, so there is nothing to complete).
    pub async fn all_graduated_epics_closed(&self, proposal_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            r#"SELECT
                    COUNT(*) AS "total!: i64",
                    COUNT(*) FILTER (WHERE e.status <> 'closed') AS "open!: i64"
               FROM proposal_epics pe
               JOIN epics e ON e.id = pe.epic_id
               WHERE pe.proposal_id = $1"#,
            proposal_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(row.total > 0 && row.open == 0)
    }

    /// Mark a proposal `done` (terminal). Stamps `closed_at` if not already set.
    /// Used by the Planner's `proposal_complete` tool after reviewing the
    /// finished build. Completing is also a successful reconcile: stamp every
    /// graduated epic at the proposal head and clear proposal-level drift before
    /// moving to the terminal state.
    pub async fn set_done(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let proposal = self.get_required(proposal_id).await?;
        let revision_seq = proposal.latest_revision_seq;
        let epics = self.graduated_epics(proposal_id).await?;
        for (epic_id, _) in epics {
            self.record_epic_reconciliation(proposal_id, &epic_id, revision_seq)
                .await?;
        }
        sqlx::query(
            r#"UPDATE proposals SET status = 'done',
                    last_reconciled_revision_seq = latest_revision_seq,
                    pending_reconcile = false,
                    reconciled_at = now(),
                    closed_at = COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
        )
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Overwrite the acceptance-criteria JSON in place — a lightweight status
    /// annotation (the Planner ticking `met` flags as epics land), NOT a spec
    /// edit. Unlike [`Self::update`], this does NOT bump `latest_revision_seq`
    /// or clear sign-offs; `ac_json` must be a JSON array string of
    /// `{criterion, met}` objects (callers merge against the current criteria
    /// to preserve the `criterion` text).
    pub async fn set_acceptance_criteria(
        &self,
        proposal_id: &str,
        ac_json: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let acceptance_criteria: serde_json::Value =
            serde_json::from_str(ac_json).map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json for proposals.acceptance_criteria: {e}"
                ))
            })?;
        sqlx::query!(
            r#"UPDATE proposals SET acceptance_criteria = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            acceptance_criteria,
            proposal_id
        )
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Apply Planner-authored acceptance-criteria amendments as real spec edits.
    ///
    /// Unlike [`Self::set_acceptance_criteria`], this bumps the proposal head
    /// revision and inserts a feedback audit entry. Unlike [`Self::update`], it
    /// intentionally retains existing sign-offs and does not demote approved
    /// proposals; the audit event is the mechanism for humans to object.
    pub async fn amend_acceptance_criteria(
        &self,
        proposal_id: &str,
        amendments: &[ProposalAcceptanceCriteriaAmendment<'_>],
        reason: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::InvalidData(
                "acceptance-criteria amendment reason is required".to_owned(),
            ));
        }
        if amendments.is_empty() {
            return Err(Error::InvalidData(
                "at least one acceptance-criteria amendment is required".to_owned(),
            ));
        }

        let current = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        let old_revision_seq = current.latest_revision_seq;
        let next_revision_seq = old_revision_seq + 1;
        let mut criteria = serde_json::from_str::<serde_json::Value>(&current.acceptance_criteria)
            .map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json in proposals.acceptance_criteria: {e}"
                ))
            })?
            .as_array()
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData("proposals.acceptance_criteria must be a JSON array".to_owned())
            })?;

        let mut audit_entries = Vec::with_capacity(amendments.len());
        for amendment in amendments {
            match amendment {
                ProposalAcceptanceCriteriaAmendment::Rewrite { index, criterion } => {
                    let criterion = criterion.trim();
                    if criterion.is_empty() {
                        return Err(Error::InvalidData(
                            "rewrite acceptance-criteria text is required".to_owned(),
                        ));
                    }
                    let old = criteria.get(*index).cloned().ok_or_else(|| {
                        Error::InvalidData(format!(
                            "acceptance-criteria index {index} out of range"
                        ))
                    })?;
                    let mut new = old.clone();
                    match &mut new {
                        serde_json::Value::Object(obj) => {
                            obj.insert(
                                "criterion".to_owned(),
                                serde_json::Value::String(criterion.to_owned()),
                            );
                        }
                        _ => new = serde_json::Value::String(criterion.to_owned()),
                    }
                    criteria[*index] = new.clone();
                    audit_entries.push(ProposalAcceptanceCriteriaAuditEntry {
                        operation: "rewrite",
                        index: *index,
                        old_criterion: old,
                        new_criterion: new,
                    });
                }
                ProposalAcceptanceCriteriaAmendment::Drop { index } => {
                    if *index >= criteria.len() {
                        return Err(Error::InvalidData(format!(
                            "acceptance-criteria index {index} out of range"
                        )));
                    }
                    let old = criteria.remove(*index);
                    audit_entries.push(ProposalAcceptanceCriteriaAuditEntry {
                        operation: "drop",
                        index: *index,
                        old_criterion: old,
                        new_criterion: serde_json::json!({"dropped": true}),
                    });
                }
                ProposalAcceptanceCriteriaAmendment::Waive { index } => {
                    let old = criteria.get(*index).cloned().ok_or_else(|| {
                        Error::InvalidData(format!(
                            "acceptance-criteria index {index} out of range"
                        ))
                    })?;
                    let mut new = old.clone();
                    match &mut new {
                        serde_json::Value::Object(obj) => {
                            obj.insert("waived".to_owned(), serde_json::Value::Bool(true));
                        }
                        _ => {
                            new = serde_json::json!({
                                "criterion": old,
                                "waived": true
                            });
                        }
                    }
                    criteria[*index] = new.clone();
                    audit_entries.push(ProposalAcceptanceCriteriaAuditEntry {
                        operation: "waive",
                        index: *index,
                        old_criterion: old,
                        new_criterion: new,
                    });
                }
            }
        }

        let acceptance_criteria = serde_json::Value::Array(criteria);
        sqlx::query(
            r#"UPDATE proposals SET acceptance_criteria = $1, latest_revision_seq = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
        )
        .bind(&acceptance_criteria)
        .bind(next_revision_seq)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let editor = djinn_core::auth_context::current_user_id();
        self.insert_revision(
            proposal_id,
            next_revision_seq,
            &current.title,
            &current.body,
            &current.body_format,
            &acceptance_criteria,
            editor.as_deref(),
        )
        .await?;

        let audit_json = serde_json::to_string(&audit_entries)
            .map_err(|e| Error::InvalidData(format!("failed to encode amendment audit: {e}")))?;
        let body = format!(
            "Acceptance criteria amended\nreason: {reason}\nrevision: {old_revision_seq} -> {next_revision_seq}\namendments: {audit_json}"
        );
        self.add_feedback(ProposalFeedbackCreateInput {
            proposal_id,
            parent_id: None,
            author_kind: "ai",
            author_model: None,
            body: &body,
            target_section: Some("acceptance_criteria"),
        })
        .await?;

        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// `building` proposals whose build has fully drained — at least one
    /// graduated epic, and every graduated epic closed — for the coordinator's
    /// backfill sweep. Catches proposals whose epics closed before the review
    /// rule existed (or whose `epic.updated` event was missed), which would
    /// otherwise sit in `building` forever with no closeout review.
    pub async fn drained_building_proposals(&self) -> Result<Vec<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id
             FROM proposals p
             WHERE p.status = 'building'
               AND EXISTS (SELECT 1 FROM proposal_epics pe WHERE pe.proposal_id = p.id)
               AND NOT EXISTS (
                   SELECT 1 FROM proposal_epics pe
                   JOIN epics e ON e.id = pe.epic_id
                   WHERE pe.proposal_id = p.id AND e.status <> 'closed'
               )
             ORDER BY p.updated_at"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// `building` proposals whose proposal head has drifted ahead of the
    /// revision stamped as reconciled into the graduated build. Used by the
    /// coordinator's reconcile backstop sweep to recover missed
    /// `proposal.updated` events.
    pub async fn drift_building_proposals(&self) -> Result<Vec<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Proposal>(
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id
             FROM proposals p
             WHERE p.status = 'building'
               AND (
                   p.pending_reconcile = true
                   OR p.latest_revision_seq > COALESCE(p.last_reconciled_revision_seq, 0)
               )
             ORDER BY p.updated_at"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    async fn get_required(&self, id: &str) -> Result<Proposal> {
        self.get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found after write: {id}")))
    }

    async fn get_feedback_required(&self, id: &str) -> Result<ProposalFeedback> {
        self.get_feedback(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("feedback not found after write: {id}")))
    }

    /// Generate a globally-unique 4-char base36 short id for proposals.
    async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::InvalidData(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::InvalidData(
            "short_id collision after 16 retries".into(),
        ))
    }
}

// ── Short ID helpers ─────────────────────────────────────────────────────────

fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616) // 36^4
}

fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

/// Global uniqueness check against the `proposals` table only (short_ids are
/// NOT per-project for proposals).
async fn short_id_exists(pool: &sqlx::PgPool, short_id: &str) -> Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM proposals WHERE short_id = $1) AS "exists!: bool""#,
        short_id
    )
    .fetch_one(pool)
    .await?)
}

// ── Dynamic query helpers ────────────────────────────────────────────────────

fn proposal_build_where(
    status: &Option<String>,
    text: &Option<String>,
    author_user_id: &Option<String>,
    target_project_id: &Option<String>,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(s) = status {
        let ph = format!("${}", params.len() + 1);
        clauses.push(format!("status = {ph}"));
        params.push(SqlParam::Text(s.clone()));
    }
    if let Some(a) = author_user_id {
        let ph = format!("${}", params.len() + 1);
        clauses.push(format!("author_user_id = {ph}"));
        params.push(SqlParam::Text(a.clone()));
    }
    if let Some(proj) = target_project_id {
        let ph = format!("${}", params.len() + 1);
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM proposal_targets pt WHERE pt.proposal_id = proposals.id AND pt.project_id = {ph})"
        ));
        params.push(SqlParam::Text(proj.clone()));
    }
    if let Some(t) = text {
        let ph_a = format!("${}", params.len() + 1);
        let ph_b = format!("${}", params.len() + 2);
        let ph_c = format!("${}", params.len() + 3);
        clauses.push(format!(
            "(title LIKE {ph_a} OR body LIKE {ph_b} OR short_id LIKE {ph_c})"
        ));
        let pattern = format!("%{t}%");
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern));
    }

    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };
    (where_sql, params)
}

fn proposal_sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        _ => "created_at DESC",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};

    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
    }

    async fn insert_project(db: &Database, owner: &str) -> String {
        // The raw insert bypasses the repository's `ensure_initialized`, so
        // clone the per-test DB from the template explicitly before using it
        // (matters when this helper is the first DB op in a test).
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
            id,
            owner,
            owner,
            format!("repo-{}", &id.replace('-', "")[..31])
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    fn create_input<'a>(title: &'a str) -> ProposalCreateInput<'a> {
        ProposalCreateInput {
            title,
            body: "",
            acceptance_criteria: None,
            status: None,
            body_format: None,
        }
    }

    fn create_input_with_ac<'a>(
        title: &'a str,
        body: &'a str,
        ac: &'a str,
    ) -> ProposalCreateInput<'a> {
        ProposalCreateInput {
            title,
            body,
            acceptance_criteria: Some(ac),
            status: None,
            body_format: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_defaults_and_short_id() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("My Proposal")).await.unwrap();
        assert_eq!(p.title, "My Proposal");
        assert_eq!(p.status, "draft");
        assert_eq!(p.short_id.len(), 4);
        assert_eq!(p.acceptance_criteria, "[]");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "proposal");
        assert_eq!(events[0].action, "created");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_by_id_and_short_id() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Resolve")).await.unwrap();
        assert_eq!(repo.resolve(&p.id).await.unwrap().unwrap().id, p.id);
        assert_eq!(repo.resolve(&p.short_id).await.unwrap().unwrap().id, p.id);
        assert!(repo.resolve("nope").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_sets_closed_at_on_archive() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Up")).await.unwrap();
        let updated = repo
            .update(
                &p.id,
                ProposalUpdateInput {
                    title: "Up2",
                    body: "new body",
                    acceptance_criteria: "[\"ac1\"]",
                    status: "archived",
                    superseded_by: None,
                    body_format: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "Up2");
        assert_eq!(updated.status, "archived");
        assert_eq!(updated.acceptance_criteria, "[\"ac1\"]");
        assert!(updated.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targets_add_rerole_remove() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p = repo.create(create_input("Targeted")).await.unwrap();
        let proj = insert_project(&db, "svc-a").await;

        assert!(repo.targets(&p.id).await.unwrap().is_empty());
        repo.add_target(&p.id, &proj, "primary").await.unwrap();
        // Re-add updates role (idempotent on the PK).
        repo.add_target(&p.id, &proj, "reference").await.unwrap();
        let targets = repo.targets(&p.id).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].role, "reference");

        repo.remove_target(&p.id, &proj).await.unwrap();
        assert!(repo.targets(&p.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feedback_add_and_resolve() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Feedback")).await.unwrap();
        captured.lock().unwrap().clear();

        // A human comment (arrives unresolved).
        let comment = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "what about X?",
                target_section: None,
            })
            .await
            .unwrap();
        assert!(comment.resolved_at.is_none());

        // An AI-authored entry.
        let ai = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "ai",
                author_model: Some("claude-opus-4-8"),
                body: "enforce in svc-invoice not the gateway",
                target_section: Some("scope"),
            })
            .await
            .unwrap();
        assert_eq!(ai.author_kind, "ai");
        assert!(ai.resolved_at.is_none());

        // Resolve the comment as addressed in revision 2.
        let resolved = repo
            .set_feedback_resolved(&comment.id, Some(2))
            .await
            .unwrap();
        assert!(resolved.resolved_at.is_some());
        assert_eq!(resolved.resolved_revision_seq, Some(2));

        // Dismiss the AI entry (no spec change).
        let dismissed = repo.set_feedback_resolved(&ai.id, None).await.unwrap();
        assert!(dismissed.resolved_at.is_some());
        assert!(dismissed.resolved_revision_seq.is_none());

        assert_eq!(repo.feedback(&p.id).await.unwrap().len(), 2);
        let events = captured.lock().unwrap();
        // two adds + two resolves = four feedback events
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|e| e.entity_type == "proposal_feedback"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_filters_by_status_and_target() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-b").await;

        let a = repo.create(create_input("Alpha")).await.unwrap();
        repo.create(create_input("Beta")).await.unwrap();
        repo.add_target(&a.id, &proj, "primary").await.unwrap();

        let all = repo
            .list_filtered(ProposalListQuery::default())
            .await
            .unwrap();
        assert_eq!(all.total_count, 2);

        let targeted = repo
            .list_filtered(ProposalListQuery {
                target_project_id: Some(proj.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(targeted.total_count, 1);
        assert_eq!(targeted.proposals[0].0.id, a.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_reports_unresolved_feedback_count() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Counted")).await.unwrap();
        let mk = |body: &'static str| ProposalFeedbackCreateInput {
            proposal_id: &p.id,
            parent_id: None,
            author_kind: "user",
            author_model: None,
            body,
            target_section: None,
        };
        let f1 = repo.add_feedback(mk("one")).await.unwrap();
        repo.add_feedback(mk("two")).await.unwrap();

        let listed = repo
            .list_filtered(ProposalListQuery::default())
            .await
            .unwrap();
        assert_eq!(listed.proposals[0].1, 2);

        // Resolving one drops the count.
        repo.set_feedback_resolved(&f1.id, Some(2)).await.unwrap();
        let listed = repo
            .list_filtered(ProposalListQuery::default())
            .await
            .unwrap();
        assert_eq!(listed.proposals[0].1, 1);
    }

    fn update_input<'a>(
        title: &'a str,
        body: &'a str,
        ac: &'a str,
        status: &'a str,
    ) -> ProposalUpdateInput<'a> {
        ProposalUpdateInput {
            title,
            body,
            acceptance_criteria: ac,
            status,
            superseded_by: None,
            body_format: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signoffs_gate_approval_revisions_and_staleness() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Gate")).await.unwrap();
        assert_eq!(p.latest_revision_seq, 1);
        // create seeds revision 1.
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);

        // Move to in_review (status-only → no new revision).
        repo.update(&p.id, update_input("Gate", "", "[]", "in_review"))
            .await
            .unwrap();
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);

        // One sign-off is not enough.
        let after_scoped = repo.add_signoff(&p.id, "scoped", "user-a").await.unwrap();
        assert_eq!(after_scoped.status, "in_review");
        // Both fresh sign-offs auto-advance to approved.
        let after_tech = repo
            .add_signoff(&p.id, "technical", "user-b")
            .await
            .unwrap();
        assert_eq!(after_tech.status, "approved");

        // Editing an approved spec demotes to in_review, bumps the revision, and
        // clears sign-offs.
        let edited = repo
            .update(&p.id, update_input("Gate v2", "", "[]", "approved"))
            .await
            .unwrap();
        assert_eq!(edited.status, "in_review");
        assert_eq!(edited.latest_revision_seq, 2);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 2);
        assert!(repo.signoffs(&p.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_only_done_appends_history_without_revision_or_signoff_staleness() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Manual done")).await.unwrap();
        repo.add_signoff(&p.id, "scoped", "user-a").await.unwrap();
        let before = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(before.status, "in_review");
        assert_eq!(before.latest_revision_seq, 1);
        let signoffs_before = repo.signoffs(&p.id).await.unwrap();
        assert_eq!(signoffs_before.len(), 1);

        let done = djinn_core::auth_context::SESSION_USER_ID
            .scope(
                Some("actor-user".to_owned()),
                repo.update(&p.id, update_input("Manual done", "", "[]", "done")),
            )
            .await
            .unwrap();

        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
        assert_eq!(done.latest_revision_seq, before.latest_revision_seq);

        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].event_kind, "spec_revision");
        let event = &revisions[1];
        assert_eq!(event.seq, before.latest_revision_seq);
        assert_eq!(event.event_kind, "status_change");
        assert_eq!(event.status_from.as_deref(), Some("in_review"));
        assert_eq!(event.status_to.as_deref(), Some("done"));
        assert_eq!(event.edited_by_user_id.as_deref(), Some("actor-user"));
        assert!(!event.created_at.is_empty());

        let signoffs_after = repo.signoffs(&p.id).await.unwrap();
        assert_eq!(signoffs_after.len(), signoffs_before.len());
        assert_eq!(
            signoffs_after[0].revision_seq,
            signoffs_before[0].revision_seq
        );
        assert_eq!(signoffs_after[0].revision_seq, done.latest_revision_seq);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signing_off_advances_a_draft_without_a_manual_status_change() {
        // Regression: a draft used to ignore sign-offs entirely — the gate only
        // fired from in_review, so a draft with both fresh sign-offs sat in
        // draft until someone manually bumped it. Signing the scope now requests
        // review, and both fresh sign-offs approve straight from draft.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Draft gate")).await.unwrap();
        assert_eq!(p.status, "draft");

        let after_scoped = repo.add_signoff(&p.id, "scoped", "user-a").await.unwrap();
        assert_eq!(after_scoped.status, "in_review");

        let after_tech = repo
            .add_signoff(&p.id, "technical", "user-b")
            .await
            .unwrap();
        assert_eq!(after_tech.status, "approved");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clearing_signoff_demotes_from_approved() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Demote")).await.unwrap();
        repo.update(&p.id, update_input("Demote", "", "[]", "in_review"))
            .await
            .unwrap();
        repo.add_signoff(&p.id, "scoped", "u1").await.unwrap();
        let approved = repo.add_signoff(&p.id, "technical", "u2").await.unwrap();
        assert_eq!(approved.status, "approved");
        let demoted = repo.clear_signoff(&p.id, "technical", "u2").await.unwrap();
        assert_eq!(demoted.status, "in_review");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn addressing_feedback_edits_spec_then_resolves_at_revision() {
        // Models the chat flow: djinn rewrites the spec via `update` (which
        // appends a revision), then marks the feedback resolved at that seq.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Edit")).await.unwrap();
        let f = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "tweak the spec",
                target_section: None,
            })
            .await
            .unwrap();
        let updated = repo
            .update(&p.id, update_input("Edit", "New spec body.", "[]", "draft"))
            .await
            .unwrap();
        assert_eq!(updated.body, "New spec body.");
        assert_eq!(updated.latest_revision_seq, 2);

        let resolved = repo
            .set_feedback_resolved(&f.id, Some(updated.latest_revision_seq))
            .await
            .unwrap();
        assert_eq!(resolved.resolved_revision_seq, Some(2));
        assert!(resolved.resolved_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn participants_and_graduation_linking() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-grad").await;
        let p = repo.create(create_input("Grad")).await.unwrap();

        repo.add_signoff(&p.id, "scoped", "user-x").await.unwrap();
        let parts = repo.participants(&p.id).await.unwrap();
        assert!(parts.contains(&"user-x".to_string()));

        // Simulate graduation linking an epic (insert an epic row directly).
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown)
             VALUES ($1, $2, 'gep1', 'T', '', '', '', 'open', '', '[]'::jsonb, true)",
            epic_id,
            proj
        )
        .execute(db.pool())
        .await
        .unwrap();
        repo.link_epic(&p.id, &epic_id, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        let graduated = repo.graduated_epics(&p.id).await.unwrap();
        assert_eq!(graduated, vec![(epic_id, proj)]);
        let built = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(built.status, "building");
        assert_eq!(built.build_owner_user_id.as_deref(), Some("user-x"));
    }

    /// Helper: insert an open epic row and return its id.
    async fn insert_epic(db: &Database, project_id: &str, short_id: &str) -> String {
        let epic_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown)
             VALUES ($1, $2, $3, 'T', '', '', '', 'open', '', '[]'::jsonb, true)",
            epic_id,
            project_id,
            short_id
        )
        .execute(db.pool())
        .await
        .unwrap();
        epic_id
    }

    async fn close_epic(db: &Database, epic_id: &str) {
        sqlx::query!("UPDATE epics SET status = 'closed' WHERE id = $1", epic_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn reconciliation_count(db: &Database, proposal_id: &str, epic_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM proposal_reconciliations WHERE proposal_id = $1 AND epic_id = $2",
        )
        .bind(proposal_id)
        .bind(epic_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_completion_lifecycle() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-done").await;
        let p = repo.create(create_input("Closeout")).await.unwrap();

        let e1 = insert_epic(&db, &proj, "ce01").await;
        let e2 = insert_epic(&db, &proj, "ce02").await;
        repo.link_epic(&p.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p.id, &e2, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        // Reverse lookup resolves the parent proposal.
        assert_eq!(repo.proposal_for_epic(&e1).await.unwrap().unwrap().id, p.id);
        assert!(
            repo.proposal_for_epic("no-such-epic")
                .await
                .unwrap()
                .is_none()
        );

        // Not complete while any graduated epic is open.
        assert!(!repo.all_graduated_epics_closed(&p.id).await.unwrap());
        close_epic(&db, &e1).await;
        assert!(!repo.all_graduated_epics_closed(&p.id).await.unwrap());
        close_epic(&db, &e2).await;
        assert!(repo.all_graduated_epics_closed(&p.id).await.unwrap());

        // set_done is terminal and stamps closed_at.
        let done = repo.set_done(&p.id).await.unwrap();
        assert_eq!(done.status, "done");
        assert!(done.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_graduated_epics_closed_is_false_without_epics() {
        // A proposal that has graduated nothing yet is not "complete".
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("No epics")).await.unwrap();
        assert!(!repo.all_graduated_epics_closed(&p.id).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_acceptance_criteria_is_a_status_annotation_not_a_spec_edit() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "AC",
                body: "",
                acceptance_criteria: Some(
                    r#"[{"criterion":"a","met":false},{"criterion":"b","met":false}]"#,
                ),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        // A sign-off anchored to the head revision.
        repo.add_signoff(&p.id, "scoped", "u1").await.unwrap();
        let seq_before = repo.get(&p.id).await.unwrap().unwrap().latest_revision_seq;

        // Mark the first criterion met.
        let updated = repo
            .set_acceptance_criteria(
                &p.id,
                r#"[{"criterion":"a","met":true},{"criterion":"b","met":false}]"#,
            )
            .await
            .unwrap();

        // Unlike update(): no new revision and the sign-off survives.
        assert_eq!(updated.latest_revision_seq, seq_before);
        assert_eq!(repo.signoffs(&p.id).await.unwrap().len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(parsed[0]["met"], serde_json::json!(true));
        assert_eq!(parsed[1]["met"], serde_json::json!(false));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn amend_acceptance_criteria_rewrites_drops_waives_and_audits() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo
            .create(ProposalCreateInput {
                title: "AC amend",
                body: "body",
                acceptance_criteria: Some(
                    r#"[{"criterion":"rewrite me","met":false},{"criterion":"drop me","met":false},{"criterion":"waive me","met":false}]"#,
                ),
                status: Some("in_review"),
                body_format: None,
            })
            .await
            .unwrap();
        repo.add_signoff(&p.id, "scoped", "u1").await.unwrap();
        let signoffs_before = repo.signoffs(&p.id).await.unwrap();
        captured.lock().unwrap().clear();

        let updated = repo
            .amend_acceptance_criteria(
                &p.id,
                &[
                    ProposalAcceptanceCriteriaAmendment::Rewrite {
                        index: 0,
                        criterion: "rewritten criterion",
                    },
                    ProposalAcceptanceCriteriaAmendment::Drop { index: 1 },
                    ProposalAcceptanceCriteriaAmendment::Waive { index: 1 },
                ],
                "criterion 2 cannot be verified by agents",
            )
            .await
            .unwrap();

        assert_eq!(updated.latest_revision_seq, p.latest_revision_seq + 1);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 2);
        let signoffs_after = repo.signoffs(&p.id).await.unwrap();
        assert_eq!(signoffs_after.len(), signoffs_before.len());
        assert_eq!(
            signoffs_after[0].proposal_id,
            signoffs_before[0].proposal_id
        );
        assert_eq!(signoffs_after[0].kind, signoffs_before[0].kind);
        assert_eq!(signoffs_after[0].user_id, signoffs_before[0].user_id);
        assert_eq!(
            signoffs_after[0].revision_seq,
            signoffs_before[0].revision_seq
        );
        let parsed: serde_json::Value = serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(
            parsed[0]["criterion"],
            serde_json::json!("rewritten criterion")
        );
        assert_eq!(parsed[0]["met"], serde_json::json!(false));
        assert_eq!(parsed[1]["criterion"], serde_json::json!("waive me"));
        assert_eq!(parsed[1]["waived"], serde_json::json!(true));

        let feedback = repo.feedback(&p.id).await.unwrap();
        assert_eq!(feedback.len(), 1);
        let audit = &feedback[0];
        assert_eq!(audit.author_kind, "ai");
        assert_eq!(audit.target_section.as_deref(), Some("acceptance_criteria"));
        assert!(
            audit
                .body
                .contains("reason: criterion 2 cannot be verified by agents")
        );
        assert!(audit.body.contains("revision: 1 -> 2"));
        let amendments_json = audit
            .body
            .strip_prefix(&format!(
                "Acceptance criteria amended\nreason: {}\nrevision: 1 -> 2\namendments: ",
                "criterion 2 cannot be verified by agents"
            ))
            .unwrap();
        let audit_entries: serde_json::Value = serde_json::from_str(amendments_json).unwrap();
        let audit_entries = audit_entries.as_array().unwrap();
        assert_eq!(audit_entries.len(), 3);
        assert_eq!(audit_entries[0]["operation"], serde_json::json!("rewrite"));
        assert_eq!(audit_entries[1]["operation"], serde_json::json!("drop"));
        assert_eq!(audit_entries[2]["operation"], serde_json::json!("waive"));
        assert_eq!(
            audit_entries[1]["old_criterion"],
            serde_json::json!({"criterion": "drop me", "met": false})
        );
        assert_eq!(
            audit_entries[1]["new_criterion"],
            serde_json::json!({"dropped": true})
        );

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].entity_type, "proposal_feedback");
        assert_eq!(events[0].action, "created");
        assert_eq!(events[1].entity_type, "proposal");
        assert_eq!(events[1].action, "updated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn amend_acceptance_criteria_validates_without_mutating() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo
            .create(ProposalCreateInput {
                title: "AC invalid",
                body: "body",
                acceptance_criteria: Some(r#"[{"criterion":"keep","met":false}]"#),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let before = repo.get(&p.id).await.unwrap().unwrap();

        let empty_reason = repo
            .amend_acceptance_criteria(
                &p.id,
                &[ProposalAcceptanceCriteriaAmendment::Rewrite {
                    index: 0,
                    criterion: "changed",
                }],
                "   ",
            )
            .await;
        assert!(empty_reason.is_err());
        let after_empty_reason = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            after_empty_reason.acceptance_criteria,
            before.acceptance_criteria
        );
        assert_eq!(
            after_empty_reason.latest_revision_seq,
            before.latest_revision_seq
        );

        let bad_index = repo
            .amend_acceptance_criteria(
                &p.id,
                &[ProposalAcceptanceCriteriaAmendment::Drop { index: 7 }],
                "bad index",
            )
            .await;
        assert!(bad_index.is_err());
        let after_bad_index = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(
            after_bad_index.acceptance_criteria,
            before.acceptance_criteria
        );
        assert_eq!(
            after_bad_index.latest_revision_seq,
            before.latest_revision_seq
        );
        assert!(repo.feedback(&p.id).await.unwrap().is_empty());
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drained_building_proposals_only_returns_fully_closed_builds() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-drain").await;

        // p1: building, one epic still OPEN → not drained.
        let p1 = repo.create(create_input("p1")).await.unwrap();
        let e1 = insert_epic(&db, &proj, "dr01").await;
        repo.link_epic(&p1.id, &e1, &proj).await.unwrap();
        repo.set_building(&p1.id, "u").await.unwrap();

        // p2: building, every epic CLOSED → drained.
        let p2 = repo.create(create_input("p2")).await.unwrap();
        let e2 = insert_epic(&db, &proj, "dr02").await;
        repo.link_epic(&p2.id, &e2, &proj).await.unwrap();
        repo.set_building(&p2.id, "u").await.unwrap();
        close_epic(&db, &e2).await;

        // p3: building, no graduated epics → not drained.
        let p3 = repo.create(create_input("p3")).await.unwrap();
        repo.set_building(&p3.id, "u").await.unwrap();

        // p4: NOT building (draft) with a closed epic → not drained.
        let p4 = repo.create(create_input("p4")).await.unwrap();
        let e4 = insert_epic(&db, &proj, "dr04").await;
        repo.link_epic(&p4.id, &e4, &proj).await.unwrap();
        close_epic(&db, &e4).await;

        let ids: Vec<String> = repo
            .drained_building_proposals()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert!(
            ids.contains(&p2.id),
            "building + all epics closed is drained"
        );
        assert!(!ids.contains(&p1.id), "an open epic means not drained");
        assert!(
            !ids.contains(&p3.id),
            "no graduated epics means not drained"
        );
        assert!(!ids.contains(&p4.id), "non-building is never drained");
    }

    /// Helper: insert an open `task` row under an epic and return its id.
    async fn insert_task(db: &Database, project_id: &str, epic_id: &str, short_id: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES ($1, $2, $3, $4, 'T', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
            id,
            project_id,
            short_id,
            epic_id
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_allows_spec_edit_while_building_and_marks_drift() {
        // Amend-while-building: a material edit to a `building` proposal is
        // allowed and only stamps drift — it does NOT touch the status. This
        // is the positive replacement for the old "spec edit while building is
        // rejected" regression; status-only updates remain allowed too.
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-editguard").await;
        let p = repo
            .create(create_input_with_ac(
                "Guarded",
                "",
                r#"[{"criterion":"do X","met":false}]"#,
            ))
            .await
            .unwrap();
        let epic = insert_epic(&db, &proj, "eg01").await;
        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        let building = repo.set_building(&p.id, "user-x").await.unwrap();
        assert_eq!(building.last_reconciled_revision_seq, Some(1));
        assert!(!building.pending_reconcile);
        assert!(
            repo.latest_epic_reconciliations(&p.id)
                .await
                .unwrap()
                .is_empty()
        );
        let linked_while_building = insert_epic(&db, &proj, "eg02").await;
        repo.link_epic(&p.id, &linked_while_building, &proj)
            .await
            .unwrap();
        let latest_by_epic = repo.latest_epic_reconciliations(&p.id).await.unwrap();
        assert_eq!(latest_by_epic.get(&linked_while_building), Some(&1));

        // Status-only update (no spec change) remains allowed: status stays
        // `building`, no new revision, no new drift.
        let status_only = repo
            .update(
                &p.id,
                update_input(
                    "Guarded",
                    "",
                    r#"[{"criterion":"do X","met":false}]"#,
                    "building",
                ),
            )
            .await
            .unwrap();
        assert_eq!(status_only.status, "building");
        assert_eq!(status_only.latest_revision_seq, 1);
        assert!(!status_only.pending_reconcile);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 1);

        // Material edit: title + body + AC all change. The build stays
        // `building`, a new revision lands, `pending_reconcile` flips true,
        // and `last_reconciled_revision_seq` does NOT advance (the build is
        // still against rev 1).
        let updated = repo
            .update(
                &p.id,
                update_input(
                    "Guarded v2",
                    "new body",
                    r#"[{"criterion":"do X better","met":false}]"#,
                    "approved",
                ),
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "Guarded v2");
        assert_eq!(updated.body, "new body");
        let ac: serde_json::Value = serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(
            ac,
            serde_json::json!([{"criterion": "do X better", "met": false}])
        );
        assert_eq!(updated.status, "building");
        assert_eq!(updated.latest_revision_seq, 2);
        assert_eq!(updated.last_reconciled_revision_seq, Some(1));
        assert!(updated.pending_reconcile);
        assert_eq!(repo.revisions(&p.id).await.unwrap().len(), 2);

        let reconciled = repo.mark_reconciled(&p.id).await.unwrap();
        assert_eq!(reconciled.last_reconciled_revision_seq, Some(2));
        assert!(!reconciled.pending_reconcile);
        let latest_by_epic = repo.latest_epic_reconciliations(&p.id).await.unwrap();
        assert_eq!(latest_by_epic.get(&epic), Some(&2));
        repo.record_epic_reconciliation(&p.id, &epic, 1)
            .await
            .unwrap();
        let latest_by_epic = repo.latest_epic_reconciliations(&p.id).await.unwrap();
        assert_eq!(latest_by_epic.get(&epic), Some(&2));

        // A closeout is itself a successful reconcile: it stamps current
        // per-epic metadata and clears proposal-level drift before moving to
        // the terminal state.
        let done = repo.set_done(&p.id).await.unwrap();
        assert_eq!(done.status, "done");
        assert!(!done.pending_reconcile);
        assert_eq!(done.last_reconciled_revision_seq, Some(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn add_signoff_while_building_does_not_reconcile_status() {
        // `reconcile_approval` short-circuits on `building` so a sign-off
        // (which would otherwise be enough to flip a draft → in_review and
        // a fresh pair → approved) never demotes a build back to the review
        // gate. The sign-off is still recorded, the status stays `building`.
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-build-signoff").await;
        let p = repo.create(create_input("Build signoff")).await.unwrap();
        let epic = insert_epic(&db, &proj, "bs01").await;
        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        let updated = repo.add_signoff(&p.id, "scoped", "user-y").await.unwrap();
        assert_eq!(updated.status, "building");
        assert_eq!(repo.signoffs(&p.id).await.unwrap().len(), 1);

        // clear_signoff is the symmetric guard: it must also skip
        // reconcile_approval on a building proposal, so withdrawing a sign-off
        // can never yank the build back to in_review.
        let cleared = repo.clear_signoff(&p.id, "scoped", "user-y").await.unwrap();
        assert_eq!(cleared.status, "building");
        assert!(repo.signoffs(&p.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlink_epics_clears_only_the_target_proposal() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-unlink").await;
        let p1 = repo.create(create_input("P1")).await.unwrap();
        let p2 = repo.create(create_input("P2")).await.unwrap();
        let e1 = insert_epic(&db, &proj, "ul01").await;
        let e2 = insert_epic(&db, &proj, "ul02").await;
        repo.link_epic(&p1.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p2.id, &e2, &proj).await.unwrap();

        repo.unlink_epics(&p1.id).await.unwrap();

        assert!(repo.graduated_epics(&p1.id).await.unwrap().is_empty());
        // p2's link is untouched.
        assert_eq!(
            repo.graduated_epics(&p2.id).await.unwrap(),
            vec![(e2, proj)]
        );
        // Idempotent: a second unlink on an already-empty proposal is a no-op.
        repo.unlink_epics(&p1.id).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unlink_epic_removes_only_requested_link_and_cascades_reconciliation() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-unlink-one").await;
        let p1 = repo.create(create_input("P1 selective")).await.unwrap();
        let p2 = repo.create(create_input("P2 untouched")).await.unwrap();
        let e1 = insert_epic(&db, &proj, "uo01").await;
        let e2 = insert_epic(&db, &proj, "uo02").await;
        let e3 = insert_epic(&db, &proj, "uo03").await;

        repo.link_epic(&p1.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p1.id, &e2, &proj).await.unwrap();
        // Link the same epic from another proposal too: the selective unlink is
        // keyed by both proposal_id and epic_id, not by epic_id alone.
        repo.link_epic(&p2.id, &e1, &proj).await.unwrap();
        repo.link_epic(&p2.id, &e3, &proj).await.unwrap();

        repo.record_epic_reconciliation(&p1.id, &e1, 1)
            .await
            .unwrap();
        repo.record_epic_reconciliation(&p1.id, &e2, 2)
            .await
            .unwrap();
        repo.record_epic_reconciliation(&p2.id, &e1, 3)
            .await
            .unwrap();
        repo.record_epic_reconciliation(&p2.id, &e3, 4)
            .await
            .unwrap();

        repo.unlink_epic(&p1.id, &e1).await.unwrap();

        assert_eq!(
            repo.graduated_epics(&p1.id).await.unwrap(),
            vec![(e2.clone(), proj.clone())]
        );
        let mut p2_links = repo.graduated_epics(&p2.id).await.unwrap();
        p2_links.sort();
        let mut expected_p2_links = vec![(e1.clone(), proj.clone()), (e3.clone(), proj.clone())];
        expected_p2_links.sort();
        assert_eq!(p2_links, expected_p2_links);

        assert_eq!(reconciliation_count(&db, &p1.id, &e1).await, 0);
        assert_eq!(reconciliation_count(&db, &p1.id, &e2).await, 1);
        assert_eq!(reconciliation_count(&db, &p2.id, &e1).await, 1);
        assert_eq!(reconciliation_count(&db, &p2.id, &e3).await, 1);

        let p1_reconciliations = repo.latest_epic_reconciliations(&p1.id).await.unwrap();
        assert_eq!(p1_reconciliations.get(&e1), None);
        assert_eq!(p1_reconciliations.get(&e2), Some(&2));
        let p2_reconciliations = repo.latest_epic_reconciliations(&p2.id).await.unwrap();
        assert_eq!(p2_reconciliations.get(&e1), Some(&3));
        assert_eq!(p2_reconciliations.get(&e3), Some(&4));

        // Idempotent: unlinking the already-removed pair again is a no-op.
        repo.unlink_epic(&p1.id, &e1).await.unwrap();
        assert_eq!(repo.graduated_epics(&p1.id).await.unwrap().len(), 1);
        assert_eq!(reconciliation_count(&db, &p1.id, &e1).await, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_frozen_round_trips() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Freeze")).await.unwrap();
        assert!(!p.build_frozen);
        let frozen = repo.set_frozen(&p.id, true).await.unwrap();
        assert!(frozen.build_frozen);
        assert!(repo.get(&p.id).await.unwrap().unwrap().build_frozen);
        let thawed = repo.set_frozen(&p.id, false).await.unwrap();
        assert!(!thawed.build_frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revert_to_approved_clears_all_build_state() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-revert").await;
        let p = repo.create(create_input("Revert")).await.unwrap();
        let epic = insert_epic(&db, &proj, "rv01").await;
        let task = insert_task(&db, &proj, &epic, "rv01t").await;

        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();
        repo.set_breakdown_task(&p.id, &task).await.unwrap();
        repo.set_frozen(&p.id, true).await.unwrap();
        let mid = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(mid.status, "building");
        assert_eq!(mid.build_breakdown_task_id.as_deref(), Some(task.as_str()));
        assert!(mid.build_frozen);

        let reverted = repo.revert_to_approved(&p.id).await.unwrap();
        assert_eq!(reverted.status, "approved");
        assert!(reverted.build_owner_user_id.is_none());
        assert!(reverted.build_breakdown_task_id.is_none());
        assert!(!reverted.build_frozen);
        // Epics are unlinked separately, not by revert.
        assert_eq!(repo.graduated_epics(&p.id).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn breakdown_task_link_survives_task_delete_as_null() {
        // ON DELETE SET NULL: hard-deleting the breakdown task nulls the link
        // rather than orphaning a dangling id or blocking the delete.
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-bd").await;
        let p = repo.create(create_input("Breakdown")).await.unwrap();
        let epic = insert_epic(&db, &proj, "bd01").await;
        let task = insert_task(&db, &proj, &epic, "bd01t").await;
        repo.set_breakdown_task(&p.id, &task).await.unwrap();
        assert_eq!(
            repo.get(&p.id)
                .await
                .unwrap()
                .unwrap()
                .build_breakdown_task_id
                .as_deref(),
            Some(task.as_str())
        );

        sqlx::query!("DELETE FROM tasks WHERE id = $1", task)
            .execute(db.pool())
            .await
            .unwrap();
        assert!(
            repo.get(&p.id)
                .await
                .unwrap()
                .unwrap()
                .build_breakdown_task_id
                .is_none()
        );
    }
}
