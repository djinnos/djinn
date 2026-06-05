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
    pub proposals: Vec<Proposal>,
    pub total_count: i64,
}

pub struct ProposalCreateInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// JSON array string of acceptance-criteria; `None` defaults to `[]`.
    pub acceptance_criteria: Option<&'a str>,
    /// Initial status; `None` defaults to `draft`.
    pub status: Option<&'a str>,
}

pub struct ProposalUpdateInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// JSON array string of acceptance-criteria.
    pub acceptance_criteria: &'a str,
    pub status: &'a str,
    pub superseded_by: Option<&'a str>,
}

pub struct ProposalFeedbackCreateInput<'a> {
    pub proposal_id: &'a str,
    pub parent_id: Option<&'a str>,
    /// `user` (default) or `ai`.
    pub author_kind: &'a str,
    pub author_model: Option<&'a str>,
    pub body: &'a str,
    pub target_section: Option<&'a str>,
    /// `None` = discussion; `open` | `accepted` | `rejected` = suggestion.
    pub status: Option<&'a str>,
    /// For an edit suggestion, the proposed new spec body.
    pub proposed_body: Option<&'a str>,
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
            r#"SELECT id, short_id, title, body,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, build_owner_user_id, build_frozen, build_breakdown_task_id
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
            r#"SELECT id, short_id, title, body,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, build_owner_user_id, build_frozen, build_breakdown_task_id
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
            r#"SELECT id, short_id, title, body,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, build_owner_user_id, build_frozen, build_breakdown_task_id
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
            "INSERT INTO proposals (id, short_id, title, body, acceptance_criteria, status, author_user_id, latest_revision_seq)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 1)",
            id,
            short_id,
            input.title,
            input.body,
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
        let current_ac: serde_json::Value =
            serde_json::from_str(&current.acceptance_criteria).unwrap_or(serde_json::json!([]));
        // A "material" edit changes the spec (title/body/AC), not just status.
        // Only material edits append a revision and disturb sign-offs.
        let content_changed = input.title != current.title
            || input.body != current.body
            || acceptance_criteria != current_ac;

        // A `building` proposal is being actively decomposed/built against the
        // current spec — editing it would silently stale the sign-offs while
        // `reconcile_approval` (which has no `building` arm) leaves the status
        // stuck on `building`. Force the operator to stop the build first
        // (proposal_stop_build → approved), then edit. Status-only updates
        // (e.g. set_done) stay allowed.
        if content_changed && current.status == "building" {
            return Err(Error::InvalidData(
                "cannot edit the spec of a proposal while it is building — \
                 stop the build first (proposal_stop_build), then edit"
                    .to_owned(),
            ));
        }

        // Stale/hard rule: editing the spec of an *approved* proposal reverts it
        // to in_review and clears its sign-offs (you changed an approved spec).
        // While in_review, edits leave sign-offs in place — they go stale
        // automatically because the head revision advances past them.
        let demote = content_changed && current.status == "approved";
        let effective_status = if demote && input.status == "approved" {
            "in_review"
        } else {
            input.status
        };
        let next_seq = if content_changed {
            current.latest_revision_seq + 1
        } else {
            current.latest_revision_seq
        };

        sqlx::query!(
            r#"UPDATE proposals SET title = $1, body = $2, acceptance_criteria = $3, status = $4,
                    superseded_by = $5, latest_revision_seq = $8,
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
            next_seq
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
                &acceptance_criteria,
                editor.as_deref(),
            )
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
        self.reconcile_approval(id).await?;
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

    // ── Feedback (unified discussion + suggestions) ──────────────────────────

    pub async fn feedback(&self, proposal_id: &str) -> Result<Vec<ProposalFeedback>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalFeedback,
            r#"SELECT id, proposal_id, parent_id, author_kind, author_user_id, author_model,
                    body, target_section, status, proposed_body, applied_revision_seq, created_at, updated_at
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
                    body, target_section, status, proposed_body, applied_revision_seq, created_at, updated_at
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
                (id, proposal_id, parent_id, author_kind, author_user_id, author_model, body, target_section, status, proposed_body)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            id,
            input.proposal_id,
            input.parent_id,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.body,
            input.target_section,
            input.status,
            input.proposed_body
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

    /// Set (or clear) the resolution status on a feedback entry. Passing
    /// `None` reverts a suggestion back to plain discussion.
    pub async fn set_feedback_status(
        &self,
        feedback_id: &str,
        status: Option<&str>,
    ) -> Result<ProposalFeedback> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposal_feedback SET status = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            status,
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

    /// Accept a feedback entry. For an edit suggestion (carries
    /// `proposed_body`), applies the proposed body to the proposal — which
    /// appends a revision through the normal edit path — and stamps the
    /// feedback with the revision it landed in. Always marks it `accepted`.
    pub async fn accept_feedback(&self, feedback_id: &str) -> Result<ProposalFeedback> {
        self.db.ensure_initialized().await?;
        let fb = self
            .get_feedback(feedback_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("feedback not found: {feedback_id}")))?;
        let mut applied_seq: Option<i32> = None;
        if let Some(proposed_body) = fb.proposed_body.as_deref() {
            let proposal = self.get(&fb.proposal_id).await?.ok_or_else(|| {
                Error::InvalidData(format!("proposal not found: {}", fb.proposal_id))
            })?;
            let updated = self
                .update(
                    &proposal.id,
                    ProposalUpdateInput {
                        title: &proposal.title,
                        body: proposed_body,
                        acceptance_criteria: &proposal.acceptance_criteria,
                        status: &proposal.status,
                        superseded_by: proposal.superseded_by.as_deref(),
                    },
                )
                .await?;
            applied_seq = Some(updated.latest_revision_seq);
        }
        sqlx::query!(
            r#"UPDATE proposal_feedback SET status = 'accepted', applied_revision_seq = $1,
                updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            applied_seq,
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
        // NOTE: dynamic SQL (WHERE + ORDER built from optional filters) — compile-time check not possible
        let sql = format!(
            r#"SELECT id, short_id, title, body, acceptance_criteria::text AS acceptance_criteria,
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, build_owner_user_id, build_frozen, build_breakdown_task_id
             FROM proposals WHERE {where_sql} ORDER BY {order_sql} LIMIT {limit_ph} OFFSET {offset_ph}"#
        );
        let mut q = sqlx::query_as::<_, Proposal>(&sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            q = q.bind(s.clone());
        }
        let proposals = q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

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
        acceptance_criteria: &serde_json::Value,
        edited_by: Option<&str>,
    ) -> Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO proposal_revisions (id, proposal_id, seq, title, body, acceptance_criteria, edited_by_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            id,
            proposal_id,
            seq,
            title,
            body,
            acceptance_criteria,
            edited_by
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Revisions of a proposal, oldest first.
    pub async fn revisions(&self, proposal_id: &str) -> Result<Vec<ProposalRevision>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalRevision,
            r#"SELECT id, proposal_id, seq, title, body,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    edited_by_user_id, created_at
             FROM proposal_revisions WHERE proposal_id = $1 ORDER BY seq"#,
            proposal_id
        )
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
        self.reconcile_approval(proposal_id).await?;
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
        self.reconcile_approval(proposal_id).await?;
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
    /// finished build.
    pub async fn set_done(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE proposals SET status = 'done',
                    closed_at = COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
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
            format!("repo-{id}")
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
    async fn feedback_discussion_and_suggestion() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Feedback")).await.unwrap();
        captured.lock().unwrap().clear();

        // Plain discussion (status None).
        let comment = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "what about X?",
                target_section: None,
                status: None,
                proposed_body: None,
            })
            .await
            .unwrap();
        assert!(comment.status.is_none());

        // Trackable suggestion (status open) then resolve it.
        let suggestion = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "ai",
                author_model: Some("claude-opus-4-8"),
                body: "enforce in svc-invoice not the gateway",
                target_section: Some("scope"),
                status: Some("open"),
                proposed_body: None,
            })
            .await
            .unwrap();
        assert_eq!(suggestion.author_kind, "ai");
        assert_eq!(suggestion.status.as_deref(), Some("open"));

        let resolved = repo
            .set_feedback_status(&suggestion.id, Some("accepted"))
            .await
            .unwrap();
        assert_eq!(resolved.status.as_deref(), Some("accepted"));

        assert_eq!(repo.feedback(&p.id).await.unwrap().len(), 2);
        let events = captured.lock().unwrap();
        // two adds + one resolve = three feedback events
        assert_eq!(events.len(), 3);
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
        assert_eq!(targeted.proposals[0].id, a.id);
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
    async fn accept_edit_suggestion_applies_body_and_appends_revision() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Edit")).await.unwrap();
        let s = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "tweak the spec",
                target_section: None,
                status: Some("open"),
                proposed_body: Some("New spec body."),
            })
            .await
            .unwrap();
        let accepted = repo.accept_feedback(&s.id).await.unwrap();
        assert_eq!(accepted.status.as_deref(), Some("accepted"));
        assert_eq!(accepted.applied_revision_seq, Some(2));
        let updated = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.body, "New spec body.");
        assert_eq!(updated.latest_revision_seq, 2);
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
        assert_eq!(
            repo.proposal_for_epic(&e1).await.unwrap().unwrap().id,
            p.id
        );
        assert!(repo.proposal_for_epic("no-such-epic").await.unwrap().is_none());

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
    async fn update_rejects_spec_edit_while_building_but_allows_status_only() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-editguard").await;
        let p = repo.create(create_input("Guarded")).await.unwrap();
        let epic = insert_epic(&db, &proj, "eg01").await;
        repo.link_epic(&p.id, &epic, &proj).await.unwrap();
        repo.set_building(&p.id, "user-x").await.unwrap();

        // A material edit while building is rejected.
        let err = repo
            .update(&p.id, update_input("Guarded v2", "new body", "[]", "building"))
            .await;
        assert!(err.is_err(), "spec edit while building must be rejected");

        // The spec is unchanged.
        let still = repo.get(&p.id).await.unwrap().unwrap();
        assert_eq!(still.title, "Guarded");
        assert_eq!(still.status, "building");

        // A status-only update (no spec change) is still allowed — e.g. set_done.
        let done = repo.set_done(&p.id).await.unwrap();
        assert_eq!(done.status, "done");
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
            repo.get(&p.id).await.unwrap().unwrap().build_breakdown_task_id.as_deref(),
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
