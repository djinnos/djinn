// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::collections::{HashMap, HashSet};

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{
    Proposal, ProposalDebateTrail, ProposalFeedback, ProposalRevision, ProposalSignoff,
    ProposalTarget,
};

use crate::database::Database;
use crate::repositories::note::NoteRepository;
use crate::repositories::note::{LexicalSearchBackend, sanitize_postgres_tsquery};
use crate::{Error, Result};

use djinn_memory::ProposalSearchResult;

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

/// Memory note reached through a proposal's graduated epics and their tasks.
///
/// This is a read-time projection, not a database model: the permalink/source
/// are read from `epics.memory_refs` / `tasks.memory_refs`, while `title` and
/// `note_type` are resolved from `notes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposalMemoryRef {
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub source_entity_type: String,
    pub source_short_id: String,
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
    /// Optional structured metadata persisted to `proposal_revisions.event_metadata`
    /// when the update triggers a material spec revision. When `None`, the
    /// revision row's `event_metadata` stays `NULL` (preserves the pre-existing
    /// behavior for ordinary `proposal_update` callers). Used by the planner
    /// refinement loop to attribute authoring revisions to the active native-skill
    /// version and to record targeted block-patch context (selector, range, etc.).
    pub event_metadata: Option<&'a serde_json::Value>,
}

pub struct ProposalFeedbackCreateInput<'a> {
    pub proposal_id: &'a str,
    pub parent_id: Option<&'a str>,
    /// `user` (default) or `ai`.
    pub author_kind: &'a str,
    pub author_model: Option<&'a str>,
    pub body: &'a str,
}

pub struct ProposalDebateTrailCreateInput<'a> {
    pub proposal_id: &'a str,
    /// `objection` | `rebuttal` | `verdict`.
    pub kind: &'a str,
    pub body: &'a str,
    /// When true, this entry blocks proposal readiness.
    pub blocking: bool,
    /// Agent role (e.g. "advocate", "adversary", "judge").
    pub agent_role: &'a str,
    /// `agent` (default) or `user`.
    pub author_kind: &'a str,
    pub author_model: Option<&'a str>,
    /// Optional source task attribution.
    pub source_task_id: Option<&'a str>,
    /// The proposal revision this entry is written against.
    pub against_revision_seq: i32,
    /// Debate round (1-based).
    pub round: i32,
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

struct ProposalRevisionSnapshot<'a> {
    proposal_id: &'a str,
    seq: i32,
    title: &'a str,
    body: &'a str,
    body_format: &'a str,
    acceptance_criteria: &'a serde_json::Value,
    edited_by: Option<&'a str>,
    /// Optional structured metadata to persist into the revision row's
    /// `event_metadata` JSONB column. `None` writes SQL `NULL` (the historical
    /// default for ordinary `proposal_update` revisions). Set by callers that
    /// need to attribute the revision to a specific source (e.g. a planner
    /// targeted block-patch attached to the active native-skill version).
    event_metadata: Option<&'a serde_json::Value>,
}

pub struct ProposalRepository {
    db: Database,
    events: EventBus,
}

impl ProposalRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Access the underlying database for constructing sibling repositories.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Access the underlying event bus for constructing sibling repositories.
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    pub async fn get(&self, id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
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
        // diff against. The seed carries no authoring metadata — the proposal
        // is brand-new, so the block-patch / native-skill attribution contract
        // does not apply.
        self.insert_revision(ProposalRevisionSnapshot {
            proposal_id: &id,
            seq: 1,
            title: input.title,
            body: input.body,
            body_format,
            acceptance_criteria: &acceptance_criteria,
            edited_by: author_user_id.as_deref(),
            event_metadata: None,
        })
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
            self.insert_revision(ProposalRevisionSnapshot {
                proposal_id: id,
                seq: next_seq,
                title: input.title,
                body: input.body,
                body_format,
                acceptance_criteria: &acceptance_criteria,
                edited_by: editor.as_deref(),
                event_metadata: input.event_metadata,
            })
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
                    body, resolved_at, resolved_revision_seq, resolved_by_user_id, created_at, updated_at
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
                    body, resolved_at, resolved_revision_seq, resolved_by_user_id, created_at, updated_at
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
                (id, proposal_id, parent_id, author_kind, author_user_id, author_model, body)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            id,
            input.proposal_id,
            input.parent_id,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.body
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

    // ── Debate trail (structured objections/rebuttals/verdicts) ──────────────

    /// List debate-trail entries for a proposal, ordered by round then creation.
    pub async fn debate_trail(&self, proposal_id: &str) -> Result<Vec<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalDebateTrail,
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
             ORDER BY round, created_at"#,
            proposal_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Get a single debate-trail entry by id.
    pub async fn get_debate_trail_entry(
        &self,
        entry_id: &str,
    ) -> Result<Option<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            ProposalDebateTrail,
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE id = $1"#,
            entry_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Append a debate-trail entry. Validates that the proposal exists and that
    /// `kind` is one of the allowed values. Emits a `proposal_debate_trail_created` event.
    pub async fn add_debate_trail_entry(
        &self,
        input: ProposalDebateTrailCreateInput<'_>,
    ) -> Result<ProposalDebateTrail> {
        self.db.ensure_initialized().await?;
        // Validate kind.
        match input.kind {
            "objection" | "rebuttal" | "verdict" => {}
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid debate trail kind: {other:?}; expected objection, rebuttal, or verdict"
                )));
            }
        }
        // Validate author_kind.
        match input.author_kind {
            "agent" | "user" => {}
            other => {
                return Err(Error::InvalidData(format!(
                    "invalid author_kind: {other:?}; expected agent or user"
                )));
            }
        }
        // Validate proposal exists.
        if self.get(input.proposal_id).await?.is_none() {
            return Err(Error::InvalidData(format!(
                "proposal not found: {}",
                input.proposal_id
            )));
        }
        let id = uuid::Uuid::now_v7().to_string();
        let author_user_id: Option<String> = if input.author_kind == "user" {
            djinn_core::auth_context::current_user_id()
        } else {
            None
        };
        sqlx::query!(
            "INSERT INTO proposal_debate_trail
                (id, proposal_id, kind, body, blocking, agent_role, author_kind,
                 author_user_id, author_model, source_task_id,
                 against_revision_seq, round)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            id,
            input.proposal_id,
            input.kind,
            input.body,
            input.blocking,
            input.agent_role,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.source_task_id,
            input.against_revision_seq,
            input.round,
        )
        .execute(self.db.pool())
        .await?;
        let entry = self.get_debate_trail_entry_required(&id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_debate_trail_created(
                input.proposal_id,
                &entry,
            ));
        Ok(entry)
    }

    /// Resolve a debate-trail entry. Stamps the resolving user via
    /// `current_user_id()`. Clears any prior reopen state. Idempotent.
    pub async fn resolve_debate_trail_entry(&self, entry_id: &str) -> Result<ProposalDebateTrail> {
        self.db.ensure_initialized().await?;
        let resolved_by = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            r#"UPDATE proposal_debate_trail SET
                    resolved_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    resolved_by_user_id = $1,
                    reopened_at = NULL,
                    reopened_by_user_id = NULL,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            resolved_by,
            entry_id
        )
        .execute(self.db.pool())
        .await?;
        let entry = self.get_debate_trail_entry_required(entry_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_debate_trail_updated(
                &entry.proposal_id,
                &entry,
            ));
        Ok(entry)
    }

    /// Reopen a previously resolved debate-trail entry. Stamps the reopening
    /// user via `current_user_id()`. No-op (idempotent) if already open.
    pub async fn reopen_debate_trail_entry(&self, entry_id: &str) -> Result<ProposalDebateTrail> {
        self.reopen_debate_trail_entry_with_user(entry_id, None)
            .await
    }

    /// Reopen a previously resolved debate-trail entry with an explicit user
    /// attribution. When `user_id` is `None`, falls back to
    /// `current_user_id()`. No-op (idempotent) if already open.
    pub async fn reopen_debate_trail_entry_with_user(
        &self,
        entry_id: &str,
        user_id: Option<&str>,
    ) -> Result<ProposalDebateTrail> {
        self.db.ensure_initialized().await?;
        let reopened_by = user_id
            .map(|s| Some(s.to_string()))
            .unwrap_or_else(djinn_core::auth_context::current_user_id);
        sqlx::query!(
            r#"UPDATE proposal_debate_trail SET
                    reopened_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    reopened_by_user_id = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2 AND resolved_at IS NOT NULL"#,
            reopened_by,
            entry_id
        )
        .execute(self.db.pool())
        .await?;
        let entry = self.get_debate_trail_entry_required(entry_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_debate_trail_updated(
                &entry.proposal_id,
                &entry,
            ));
        Ok(entry)
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim,
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

    #[allow(clippy::too_many_arguments)]
    async fn insert_revision(&self, revision: ProposalRevisionSnapshot<'_>) -> Result<()> {
        let id = uuid::Uuid::now_v7().to_string();
        // When the caller passes no event_metadata (the common case for ordinary
        // `proposal_update` writes), bind SQL NULL so the column stays empty —
        // preserving the historical shape. A non-None payload is stored as JSONB
        // for downstream attribution (targeted block-patch selection, native-skill
        // version, etc.). We persist `serde_json::Value` directly: `sqlx`'s Pg
        // encoder maps `Value::Null` to a SQL NULL, while any object/array is
        // sent as the underlying JSON literal.
        let metadata: Option<serde_json::Value> = revision.event_metadata.cloned();
        sqlx::query(
            r#"INSERT INTO proposal_revisions
                (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'spec_revision', $9)"#,
        )
        .bind(id)
        .bind(revision.proposal_id)
        .bind(revision.seq)
        .bind(revision.title)
        .bind(revision.body)
        .bind(revision.body_format)
        .bind(revision.acceptance_criteria)
        .bind(revision.edited_by)
        .bind(metadata)
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

    /// Record a refinement lifecycle event (`refinement_start` or
    /// `refinement_stop`) as a lightweight `proposal_revisions` row. These
    /// events carry `event_metadata` with structured JSON (e.g.
    /// `{ "update_authority": "checkpoint" }` or
    /// `{ "stop_reason": "adversary_dry" }`) but no spec snapshot — `title`,
    /// `body`, etc. are empty. The row's `seq` is set to the proposal's current
    /// head revision so ordering stays correct.
    pub async fn record_refinement_lifecycle(
        &self,
        proposal_id: &str,
        event_kind: &str,
        event_metadata: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let proposal = self
            .get(proposal_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("proposal not found: {proposal_id}")))?;
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            r#"INSERT INTO proposal_revisions
                (id, proposal_id, seq, title, body, body_format, acceptance_criteria, edited_by_user_id, event_kind, event_metadata)
               VALUES ($1, $2, $3, '', '', 'markdown', '[]', NULL, $4, $5)"#,
        )
        .bind(id)
        .bind(proposal_id)
        .bind(proposal.latest_revision_seq)
        .bind(event_kind)
        .bind(event_metadata)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Return the proposal IDs whose refinement is currently dangling — i.e.
    /// they have more `refinement_start` lifecycle events than `refinement_stop`
    /// events, so a refinement was started but never recorded as stopped.
    ///
    /// On a clean run this is exactly the set the coordinator is actively
    /// driving in memory. After a server restart the in-memory loops are lost
    /// but these DB rows remain, leaving "zombie" refinements that report
    /// `active` yet make no progress. Startup recovery uses this to reconcile
    /// them.
    pub async fn dangling_refinement_proposal_ids(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        let ids = sqlx::query_scalar::<_, String>(
            r#"SELECT proposal_id
               FROM proposal_revisions
               WHERE event_kind IN ('refinement_start', 'refinement_stop')
               GROUP BY proposal_id
               HAVING SUM(CASE WHEN event_kind = 'refinement_start' THEN 1 ELSE 0 END)
                    > SUM(CASE WHEN event_kind = 'refinement_stop' THEN 1 ELSE 0 END)"#,
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(ids)
    }

    /// Find the latest verdict override for a proposal. Returns
    /// `Some((override_on_revision_seq, override_metadata_json))` when an
    /// active override exists, or `None` when no override has been recorded.
    ///
    /// Gate composition (task cuzf) uses this to check whether a human
    /// override supersedes a judge `needs-work` verdict: the override is
    /// active when its `override_on_revision_seq` equals the proposal's
    /// current `latest_revision_seq`.
    pub async fn latest_verdict_override(
        &self,
        proposal_id: &str,
    ) -> Result<Option<(i32, String)>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_scalar::<_, Option<String>>(
            r#"SELECT event_metadata::text FROM proposal_revisions
               WHERE proposal_id = $1
                 AND event_kind = 'verdict_override'
               ORDER BY created_at DESC, id DESC
               LIMIT 1"#,
        )
        .bind(proposal_id)
        .fetch_optional(self.db.pool())
        .await?;
        if let Some(Some(meta_str)) = row {
            // Extract override_on_revision_seq from the JSON.
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta_str)
                && let Some(seq) = meta
                    .get("override_on_revision_seq")
                    .and_then(|v| v.as_i64())
            {
                return Ok(Some((seq as i32, meta_str)));
            }
        }
        Ok(None)
    }

    /// Patch the `event_metadata` column on the latest `spec_revision` row for
    /// `proposal_id`.  Used by the refinement coordinator to retroactively
    /// attribute an advocate-authored revision after the agent session completes
    /// (the agent's `proposal_update` tool call doesn't carry refinement
    /// context, so the metadata is set post-hoc).
    ///
    /// When no `spec_revision` row exists for the given `seq`, this is a
    /// no-op — the revision was created by a non-spec source (lifecycle event,
    /// status change, etc.) and doesn't need attribution.
    pub async fn set_latest_revision_event_metadata(
        &self,
        proposal_id: &str,
        seq: i32,
        event_metadata: &serde_json::Value,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let metadata: Option<serde_json::Value> = Some(event_metadata.clone());
        sqlx::query(
            r#"UPDATE proposal_revisions
               SET event_metadata = $3
             WHERE proposal_id = $1 AND seq = $2 AND event_kind = 'spec_revision'"#,
        )
        .bind(proposal_id)
        .bind(seq)
        .bind(metadata)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Return all `spec_revision` rows whose `event_metadata` marks them as
    /// `checkpoint_pending` — i.e. advocate revisions produced in checkpoint
    /// mode that have not yet been approved or rejected.
    ///
    /// Rows are ordered newest-first so the UI can surface the most recent
    /// pending revision at the top.
    pub async fn pending_checkpoint_revisions(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalRevision>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalRevision>(
            r#"SELECT id, proposal_id, seq, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    edited_by_user_id, event_kind, status_from, status_to,
                    event_metadata::text AS event_metadata, created_at
             FROM proposal_revisions
             WHERE proposal_id = $1
               AND event_kind = 'spec_revision'
               AND event_metadata->>'checkpoint_status' = 'pending'
             ORDER BY created_at DESC, id DESC"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Approve a pending checkpoint revision: apply its body/title/AC to the
    /// live proposal, advance the head revision, and mark the revision row as
    /// `checkpoint_approved`.
    ///
    /// Idempotent: if the revision is already approved or rejected, this is a
    /// no-op that returns the current proposal unchanged.
    ///
    /// `approved_by_user_id` is recorded in the event_metadata for audit.
    pub async fn approve_checkpoint_revision(
        &self,
        proposal_id: &str,
        revision_seq: i32,
        approved_by_user_id: Option<&str>,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;

        // 1. Load the pending revision row.
        let revision = sqlx::query_as::<_, ProposalRevision>(
            r#"SELECT id, proposal_id, seq, title, body, body_format,
                    acceptance_criteria::text AS acceptance_criteria,
                    edited_by_user_id, event_kind, status_from, status_to,
                    event_metadata::text AS event_metadata, created_at
             FROM proposal_revisions
             WHERE proposal_id = $1 AND seq = $2
               AND event_kind = 'spec_revision'
               AND event_metadata->>'checkpoint_status' = 'pending'"#,
        )
        .bind(proposal_id)
        .bind(revision_seq)
        .fetch_optional(self.db.pool())
        .await?;

        let Some(revision) = revision else {
            // No pending revision at this seq — idempotent no-op.
            return self.get_required(proposal_id).await;
        };

        // 2. Apply the revision's body/title/AC to the live proposal.
        let ac: serde_json::Value =
            serde_json::from_str(&revision.acceptance_criteria).unwrap_or(serde_json::json!([]));
        let new_seq = {
            let current = self.get_required(proposal_id).await?;
            current.latest_revision_seq + 1
        };
        sqlx::query(
            r#"UPDATE proposals SET title = $1, body = $2, body_format = $3,
                    acceptance_criteria = $4, latest_revision_seq = $5,
                    pending_reconcile = true,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $6"#,
        )
        .bind(&revision.title)
        .bind(&revision.body)
        .bind(&revision.body_format)
        .bind(&ac)
        .bind(new_seq)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;

        // 3. Insert a new spec_revision row for the approval event so history
        //    records the apply.
        self.insert_revision(ProposalRevisionSnapshot {
            proposal_id,
            seq: new_seq,
            title: &revision.title,
            body: &revision.body,
            body_format: &revision.body_format,
            acceptance_criteria: &ac,
            edited_by: approved_by_user_id,
            event_metadata: Some(&serde_json::json!({
                "source": "checkpoint_approval",
                "approved_from_seq": revision_seq,
                "approved_by": approved_by_user_id,
            })),
        })
        .await?;

        // 4. Mark the original pending row as `checkpoint_approved`.
        let approved_meta = {
            let mut meta: serde_json::Value = revision
                .event_metadata
                .as_ref()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or(serde_json::json!({}));
            meta["checkpoint_status"] = serde_json::json!("approved");
            if let Some(uid) = approved_by_user_id {
                meta["approved_by_user_id"] = serde_json::json!(uid);
            }
            meta
        };
        sqlx::query(
            r#"UPDATE proposal_revisions
               SET event_metadata = $3
             WHERE proposal_id = $1 AND seq = $2 AND event_kind = 'spec_revision'"#,
        )
        .bind(proposal_id)
        .bind(revision_seq)
        .bind(&approved_meta)
        .execute(self.db.pool())
        .await?;

        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Reject a pending checkpoint revision: mark it as `checkpoint_rejected`
    /// without modifying the live proposal body.
    ///
    /// Idempotent: if the revision is already approved or rejected, this is a
    /// no-op that returns the current proposal unchanged.
    ///
    /// `rejected_by_user_id` is recorded in the event_metadata for audit.
    pub async fn reject_checkpoint_revision(
        &self,
        proposal_id: &str,
        revision_seq: i32,
        rejected_by_user_id: Option<&str>,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;

        // Mark the pending row as `checkpoint_rejected` (no body mutation).
        let rejected_meta = {
            let existing = sqlx::query_scalar::<_, Option<String>>(
                r#"SELECT event_metadata::text FROM proposal_revisions
                   WHERE proposal_id = $1 AND seq = $2
                     AND event_kind = 'spec_revision'
                     AND event_metadata->>'checkpoint_status' = 'pending'"#,
            )
            .bind(proposal_id)
            .bind(revision_seq)
            .fetch_optional(self.db.pool())
            .await?;

            let Some(existing_str) = existing.flatten() else {
                // No pending revision — idempotent no-op.
                return self.get_required(proposal_id).await;
            };

            let mut meta: serde_json::Value =
                serde_json::from_str(&existing_str).unwrap_or(serde_json::json!({}));
            meta["checkpoint_status"] = serde_json::json!("rejected");
            if let Some(uid) = rejected_by_user_id {
                meta["rejected_by_user_id"] = serde_json::json!(uid);
            }
            meta
        };

        sqlx::query(
            r#"UPDATE proposal_revisions
               SET event_metadata = $3
             WHERE proposal_id = $1 AND seq = $2 AND event_kind = 'spec_revision'"#,
        )
        .bind(proposal_id)
        .bind(revision_seq)
        .bind(&rejected_meta)
        .execute(self.db.pool())
        .await?;

        self.get_required(proposal_id).await
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
        let had_graduated_epics = !self.graduated_epics(proposal_id).await?.is_empty();
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
            let seq = if had_graduated_epics {
                proposal
                    .last_reconciled_revision_seq
                    .unwrap_or(proposal.latest_revision_seq)
            } else {
                proposal.latest_revision_seq
            };
            self.record_epic_reconciliation(proposal_id, epic_id, seq)
                .await?;
            if !had_graduated_epics {
                self.mark_reconciled(proposal_id).await?;
            }
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

    /// Memory notes attached to this proposal's graduated epics or their tasks.
    ///
    /// Walks `proposal_epics -> epics.memory_refs` and then each graduated
    /// epic's `tasks.memory_refs`, resolving note metadata from the `notes`
    /// table. Duplicate permalinks are returned once, keeping the first source
    /// encountered in graduation/task order.
    pub async fn memory_refs_for_proposal(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalMemoryRef>> {
        self.db.ensure_initialized().await?;
        let note_repo = NoteRepository::new(self.db.clone(), self.events.clone());
        let mut seen = HashSet::new();
        let mut refs = Vec::new();

        for (epic_id, project_id) in self.graduated_epics(proposal_id).await? {
            let epic = sqlx::query_as::<_, (String, String)>(
                r#"SELECT short_id, memory_refs::text
                   FROM epics
                   WHERE id = $1"#,
            )
            .bind(&epic_id)
            .fetch_optional(self.db.pool())
            .await?;

            if let Some((epic_short_id, epic_memory_refs)) = epic {
                for permalink in parse_memory_refs_json(&epic_memory_refs)? {
                    if let Some(note) = note_repo.get_by_permalink(&project_id, &permalink).await?
                        && seen.insert(permalink.clone())
                    {
                        refs.push(ProposalMemoryRef {
                            permalink,
                            title: note.title,
                            note_type: note.note_type,
                            source_entity_type: "epic".to_owned(),
                            source_short_id: epic_short_id.clone(),
                        });
                    }
                }

                let task_rows = sqlx::query_as::<_, (String, String)>(
                    r#"SELECT short_id, memory_refs::text
                       FROM tasks
                       WHERE epic_id = $1
                       ORDER BY created_at, id"#,
                )
                .bind(&epic_id)
                .fetch_all(self.db.pool())
                .await?;

                for (task_short_id, task_memory_refs) in task_rows {
                    for permalink in parse_memory_refs_json(&task_memory_refs)? {
                        if let Some(note) =
                            note_repo.get_by_permalink(&project_id, &permalink).await?
                            && seen.insert(permalink.clone())
                        {
                            refs.push(ProposalMemoryRef {
                                permalink,
                                title: note.title,
                                note_type: note.note_type,
                                source_entity_type: "task".to_owned(),
                                source_short_id: task_short_id.clone(),
                            });
                        }
                    }
                }
            }
        }

        Ok(refs)
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

    /// Park the proposal for a needs-evidence spike: move status back to
    /// `draft`, link the spike task, and record the named feasibility claim.
    /// Emits a `proposal_updated` event.
    pub async fn set_needs_evidence_spike(
        &self,
        proposal_id: &str,
        spike_task_id: &str,
        claim: &str,
    ) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE proposals SET
                    status = 'draft',
                    linked_spike_task_id = $1,
                    needs_evidence_claim = $2,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
        )
        .bind(spike_task_id)
        .bind(claim)
        .bind(proposal_id)
        .execute(self.db.pool())
        .await?;
        let proposal = self.get_required(proposal_id).await?;
        self.events
            .send(DjinnEventEnvelope::proposal_updated(&proposal));
        Ok(proposal)
    }

    /// Clear the needs-evidence spike linkage after the spike closes and
    /// refinement resumes. Emits a `proposal_updated` event.
    pub async fn clear_needs_evidence_spike(&self, proposal_id: &str) -> Result<Proposal> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"UPDATE proposals SET
                    linked_spike_task_id = NULL,
                    needs_evidence_claim = NULL,
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

    /// Find a proposal that is parked on the given spike task (reverse lookup
    /// from spike task id to proposal). Returns `None` when no proposal is
    /// parked on this spike.
    pub async fn find_by_linked_spike(&self, spike_task_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Proposal,
            r#"SELECT id, short_id, title, body, body_format,
                    acceptance_criteria::text AS "acceptance_criteria!",
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
             FROM proposals WHERE linked_spike_task_id = $1"#,
            spike_task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
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

    /// The proposal whose graduation/breakdown Planner task is `task_id`, if any.
    ///
    /// Initial proposal-decomposition sessions run on the proposal's
    /// `build_breakdown_task_id` before any child epic exists, so they cannot be
    /// reached through [`Self::proposal_for_epic`]. This reverse lookup lets
    /// session extraction attach planner-read provenance notes to the proposal
    /// as soon as that local task/session data is available.
    pub async fn proposal_for_breakdown_task(&self, task_id: &str) -> Result<Option<Proposal>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT id FROM proposals WHERE build_breakdown_task_id = $1 LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?;
        match row {
            Some((id,)) => self.get(&id).await,
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
        self.insert_revision(ProposalRevisionSnapshot {
            proposal_id,
            seq: next_revision_seq,
            title: &current.title,
            body: &current.body,
            body_format: &current.body_format,
            acceptance_criteria: &acceptance_criteria,
            edited_by: editor.as_deref(),
            // AC amendments stamp a separate `proposal_feedback` audit entry
            // that holds the structured change list — no extra metadata needed
            // on the spec revision itself.
            event_metadata: None,
        })
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at, latest_revision_seq, last_reconciled_revision_seq, pending_reconcile, build_owner_user_id, build_frozen, build_breakdown_task_id, linked_spike_task_id, needs_evidence_claim
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

    async fn get_debate_trail_entry_required(&self, id: &str) -> Result<ProposalDebateTrail> {
        self.get_debate_trail_entry(id).await?.ok_or_else(|| {
            Error::InvalidData(format!("debate trail entry not found after write: {id}"))
        })
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

    /// Full-text search across proposals using the `search_vector` tsvector
    /// column (Postgres) or a LIKE fallback (SQLite).
    ///
    /// Returns proposals ranked by BM25/ts_rank, filtered to exclude archived
    /// and rejected proposals. Each result includes an HTML snippet with
    /// `<b>...</b>` highlights around matched terms.
    pub async fn search_proposals(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        self.db.ensure_initialized().await?;

        let backend = match self.db.backend_capabilities().lexical_search {
            crate::database::NoteSearchBackend::SqliteFts5 => LexicalSearchBackend::SqliteFts5,
            crate::database::NoteSearchBackend::PostgresTsvector => {
                LexicalSearchBackend::PostgresTsvector
            }
        };

        match backend {
            LexicalSearchBackend::PostgresTsvector => {
                self.search_proposals_postgres(query, limit).await
            }
            LexicalSearchBackend::SqliteFts5 => self.search_proposals_sqlite(query, limit).await,
        }
    }

    /// Postgres path: tsvector GIN index with ts_rank + ts_headline.
    async fn search_proposals_postgres(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        let sanitized = match sanitize_postgres_tsquery(query) {
            Some(q) => q,
            None => return Ok(vec![]),
        };

        // NOTE: dynamic SQL (backend-specific FTS query) — compile-time check not possible
        let sql = r#"SELECT id, short_id, title, status,
                ts_headline('english', body, to_tsquery('english', $1),
                            'StartSel=<b>, StopSel=</b>, MaxFragments=2, MaxWords=40, MinWords=20')
                    AS snippet,
                ts_rank(search_vector, to_tsquery('english', $1))::float8 AS score
             FROM proposals
             WHERE search_vector @@ to_tsquery('english', $1)
               AND status NOT IN ('archived', 'rejected')
             ORDER BY score DESC, id ASC
             LIMIT $2"#;

        let rows =
            sqlx::query_as::<sqlx::Postgres, (String, String, String, String, String, f64)>(sql)
                .bind(&sanitized)
                .bind(limit as i64)
                .fetch_all(self.db.pool())
                .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, short_id, title, status, snippet, score)| ProposalSearchResult {
                    id,
                    short_id,
                    title,
                    status,
                    snippet,
                    score,
                },
            )
            .collect())
    }

    /// SQLite fallback: LIKE queries against title + body + acceptance_criteria.
    async fn search_proposals_sqlite(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        let tokens: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .take(12)
            .collect();

        if tokens.is_empty() {
            return Ok(vec![]);
        }

        // Build LIKE conditions — each token must appear somewhere in the
        // concatenated searchable text.
        let mut conditions = Vec::new();
        for i in 0..tokens.len() {
            conditions.push(format!("(title || ' ' || body || ' ' || COALESCE(acceptance_criteria::text, '')) ILIKE ${}", i + 3));
        }
        let where_clause = conditions.join(" AND ");

        let sql = format!(
            r#"SELECT id, short_id, title, status,
                    substr(body, 1, 200) AS snippet,
                    1.0 AS score
             FROM proposals
             WHERE {}
               AND status NOT IN ('archived', 'rejected')
             ORDER BY updated_at DESC
             LIMIT $2"#,
            where_clause
        );

        let mut q =
            sqlx::query_as::<sqlx::Postgres, (String, String, String, String, String, f64)>(&sql);
        for token in &tokens {
            let pattern = format!("%{}%", token);
            q = q.bind(pattern);
        }
        q = q.bind(limit as i64);

        let rows = q.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, short_id, title, status, snippet, score)| ProposalSearchResult {
                    id,
                    short_id,
                    title,
                    status,
                    snippet,
                    score,
                },
            )
            .collect())
    }

    // ── Composed gate helpers (task cuzf) ─────────────────────────────

    /// List unresolved blocking debate-trail entries for a proposal.
    ///
    /// An entry is "unresolved" when `resolved_at IS NULL` OR
    /// (`resolved_at IS NOT NULL AND reopened_at IS NOT NULL`).
    /// Only `blocking = true` entries are returned.
    pub async fn list_unresolved_blocking_debate_entries(
        &self,
        proposal_id: &str,
    ) -> Result<Vec<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalDebateTrail>(
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
               AND blocking = true
               AND (resolved_at IS NULL
                    OR (resolved_at IS NOT NULL AND reopened_at IS NOT NULL))
             ORDER BY round, created_at"#,
        )
        .bind(proposal_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the latest judge verdict entry for a proposal.
    ///
    /// Looks for debate-trail entries with `kind = 'verdict'` and
    /// `agent_role = 'judge'`, ordered newest-first.
    pub async fn latest_judge_verdict(
        &self,
        proposal_id: &str,
    ) -> Result<Option<ProposalDebateTrail>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ProposalDebateTrail>(
            r#"SELECT id, proposal_id, kind, body, blocking, agent_role, author_kind,
                    author_user_id, author_model, source_task_id,
                    against_revision_seq, round,
                    resolved_at, resolved_by_user_id,
                    reopened_at, reopened_by_user_id,
                    created_at, updated_at
             FROM proposal_debate_trail
             WHERE proposal_id = $1
               AND kind = 'verdict'
               AND agent_role = 'judge'
             ORDER BY created_at DESC, id DESC
             LIMIT 1"#,
        )
        .bind(proposal_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Check whether a proposal is currently parked on an open
    /// needs-evidence spike.
    pub async fn has_open_needs_evidence_spike(&self, proposal_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) AS "n!: i64" FROM proposals
             WHERE id = $1
               AND linked_spike_task_id IS NOT NULL"#,
        )
        .bind(proposal_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(count > 0)
    }

    /// Check whether any pending checkpoint revisions exist.
    pub async fn has_pending_checkpoint_revisions(&self, proposal_id: &str) -> Result<bool> {
        let pending = self.pending_checkpoint_revisions(proposal_id).await?;
        Ok(!pending.is_empty())
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

fn parse_memory_refs_json(memory_refs_json: &str) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(memory_refs_json).map_err(|e| {
        Error::InvalidData(format!("invalid json for proposal memory_refs walk: {e}"))
    })?;
    Ok(value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
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
                    event_metadata: None,
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
            event_metadata: None,
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

    async fn set_epic_memory_refs(db: &Database, epic_id: &str, refs: Vec<String>) {
        let memory_refs =
            serde_json::Value::Array(refs.into_iter().map(serde_json::Value::String).collect());
        sqlx::query("UPDATE epics SET memory_refs = $1 WHERE id = $2")
            .bind(memory_refs)
            .bind(epic_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    async fn set_task_memory_refs(db: &Database, task_id: &str, refs: Vec<String>) {
        let memory_refs =
            serde_json::Value::Array(refs.into_iter().map(serde_json::Value::String).collect());
        sqlx::query("UPDATE tasks SET memory_refs = $1 WHERE id = $2")
            .bind(memory_refs)
            .bind(task_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_refs_for_proposal_walks_epics_and_tasks_deduping() {
        let db = test_db();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
        let proj = insert_project(&db, "svc-memory-walk").await;
        let proposal = repo.create(create_input("Memory walk")).await.unwrap();

        let epic_one_ref = note_repo
            .create(&proj, "Epic One Ref", "epic one", "case", "[]")
            .await
            .unwrap();
        let epic_two_ref = note_repo
            .create(&proj, "Epic Two Ref", "epic two", "pattern", "[]")
            .await
            .unwrap();
        let task_one_ref = note_repo
            .create(&proj, "Task One Ref", "task one", "pitfall", "[]")
            .await
            .unwrap();
        let task_two_ref = note_repo
            .create(&proj, "Task Two Ref", "task two", "adr", "[]")
            .await
            .unwrap();

        let epic_one = insert_epic(&db, &proj, "mr01").await;
        let epic_two = insert_epic(&db, &proj, "mr02").await;
        repo.link_epic(&proposal.id, &epic_one, &proj)
            .await
            .unwrap();
        repo.link_epic(&proposal.id, &epic_two, &proj)
            .await
            .unwrap();

        set_epic_memory_refs(
            &db,
            &epic_one,
            vec![
                epic_one_ref.permalink.clone(),
                task_one_ref.permalink.clone(),
            ],
        )
        .await;
        set_epic_memory_refs(
            &db,
            &epic_two,
            vec![
                epic_two_ref.permalink.clone(),
                epic_one_ref.permalink.clone(),
            ],
        )
        .await;

        let task_one = insert_task(&db, &proj, &epic_one, "mt01").await;
        let task_two = insert_task(&db, &proj, &epic_one, "mt02").await;
        let task_three = insert_task(&db, &proj, &epic_two, "mt03").await;
        let task_four = insert_task(&db, &proj, &epic_two, "mt04").await;
        set_task_memory_refs(
            &db,
            &task_one,
            vec![
                task_one_ref.permalink.clone(),
                task_two_ref.permalink.clone(),
            ],
        )
        .await;
        set_task_memory_refs(&db, &task_two, vec![epic_one_ref.permalink.clone()]).await;
        set_task_memory_refs(&db, &task_three, vec![task_two_ref.permalink.clone()]).await;
        set_task_memory_refs(&db, &task_four, vec![epic_two_ref.permalink.clone()]).await;

        let refs = repo.memory_refs_for_proposal(&proposal.id).await.unwrap();
        assert_eq!(refs.len(), 4);

        let by_permalink: HashMap<_, _> = refs
            .into_iter()
            .map(|memory_ref| (memory_ref.permalink.clone(), memory_ref))
            .collect();

        assert_eq!(
            by_permalink.get(&epic_one_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: epic_one_ref.permalink.clone(),
                title: "Epic One Ref".to_owned(),
                note_type: "case".to_owned(),
                source_entity_type: "epic".to_owned(),
                source_short_id: "mr01".to_owned(),
            }
        );
        assert_eq!(
            by_permalink.get(&task_one_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: task_one_ref.permalink.clone(),
                title: "Task One Ref".to_owned(),
                note_type: "pitfall".to_owned(),
                source_entity_type: "epic".to_owned(),
                source_short_id: "mr01".to_owned(),
            }
        );
        assert_eq!(
            by_permalink.get(&task_two_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: task_two_ref.permalink.clone(),
                title: "Task Two Ref".to_owned(),
                note_type: "adr".to_owned(),
                source_entity_type: "task".to_owned(),
                source_short_id: "mt01".to_owned(),
            }
        );
        assert_eq!(
            by_permalink.get(&epic_two_ref.permalink).unwrap(),
            &ProposalMemoryRef {
                permalink: epic_two_ref.permalink.clone(),
                title: "Epic Two Ref".to_owned(),
                note_type: "pattern".to_owned(),
                source_entity_type: "epic".to_owned(),
                source_short_id: "mr02".to_owned(),
            }
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_proposals_returns_matching_proposal_with_nonzero_score() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        // Create a proposal with a unique sentinel word in its body.
        let sentinel = format!("xyzzy{}searchtest", uuid::Uuid::now_v7().as_simple());
        let p = repo
            .create(ProposalCreateInput {
                title: "Search Target Proposal",
                body: &format!(
                    "This proposal describes a {} integration pattern for the platform.",
                    sentinel
                ),
                acceptance_criteria: Some(r#"[{"criterion":"works","met":false}]"#),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .unwrap();

        let results = repo.search_proposals(&sentinel, 10).await.unwrap();

        assert!(
            !results.is_empty(),
            "search_proposals should return at least one result for a unique sentinel word"
        );
        let hit = results
            .iter()
            .find(|r| r.short_id == p.short_id)
            .expect("the created proposal should appear in search results");
        assert!(
            hit.score > 0.0,
            "the ts_rank score for an exact match should be positive"
        );
        assert!(
            !hit.snippet.is_empty(),
            "the snippet should not be empty for a matching proposal"
        );
        assert_eq!(hit.title, "Search Target Proposal");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn search_proposals_excludes_archived_and_rejected() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let sentinel = format!("zyxxy{}excludetest", uuid::Uuid::now_v7().as_simple());
        let p = repo
            .create(ProposalCreateInput {
                title: "Excluded Proposal",
                body: &format!("Body with {} keyword.", sentinel),
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Verify it appears when active (draft).
        let results = repo.search_proposals(&sentinel, 10).await.unwrap();
        assert!(results.iter().any(|r| r.short_id == p.short_id));

        // Archive it.
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Excluded Proposal",
                body: &format!("Body with {} keyword.", sentinel),
                acceptance_criteria: "[]",
                status: "archived",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let results = repo.search_proposals(&sentinel, 10).await.unwrap();
        assert!(
            !results.iter().any(|r| r.short_id == p.short_id),
            "archived proposals should not appear in search results"
        );
    }

    // ── Material revision metadata (event_metadata) plumbing ──────────────────
    //
    // The block-patch primitive and the planner refinement loop depend on the
    // repository persisting structured metadata on the spec revision row so
    // each targeted patch (and the native-skill version that produced it) is
    // attributable after the fact. These tests pin the contract:
    //
    //   * The create seed revision and ordinary `proposal_update` calls write
    //     `event_metadata = NULL` (backward compatible — no schema change, no
    //     contract drift for existing callers).
    //   * When a caller passes a `serde_json::Value` through
    //     `ProposalUpdateInput { event_metadata, .. }`, the same value is
    //     round-tripped through `ProposalRepository::revisions`.
    //   * Status-only updates and the `status_change` audit events keep
    //     `event_metadata = NULL` (audit history stays metadata-free).
    //   * A `proposal_create` followed by an `update` with metadata produces
    //     a head revision whose metadata survives the read path unchanged.

    /// The seed revision written by `ProposalRepository::create` must keep
    /// `event_metadata` NULL. The seed is not an authoring operation — the
    /// block-patch / native-skill attribution contract does not apply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_seed_revision_has_null_event_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Seed Meta")).await.unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 1, "create must seed exactly one revision");
        let seed = &revisions[0];
        assert_eq!(seed.seq, 1);
        assert_eq!(seed.event_kind, "spec_revision");
        assert!(
            seed.event_metadata.is_none(),
            "create seed revision must leave event_metadata NULL, got {:?}",
            seed.event_metadata
        );
    }

    /// Ordinary `proposal_update` (no `event_metadata` payload) must keep the
    /// `event_metadata` column NULL. This is the backward-compatibility
    /// contract the task description pins for every existing caller
    /// (`proposal_update`, `proposal_create`, `proposal_import`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordinary_update_writes_null_event_metadata() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Backward Compat")).await.unwrap();
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Backward Compat v2",
                body: "v2 body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 2);
        let head = revisions.last().expect("head revision");
        assert_eq!(head.seq, 2);
        assert!(
            head.event_metadata.is_none(),
            "ordinary proposal_update must keep event_metadata NULL, got {:?}",
            head.event_metadata
        );
    }

    /// When the caller supplies structured metadata through
    /// `ProposalUpdateInput { event_metadata, .. }`, the repository must
    /// persist it into the `proposal_revisions.event_metadata` JSONB column
    /// unchanged (stable JSON shape) and the read path must surface the same
    /// text. The metadata is the typed contract that future targeted-patch
    /// calls will build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_with_event_metadata_round_trips_to_revision() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Patchy")).await.unwrap();
        let metadata = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "block_id": "callout-tip",
            "selector": "paragraph: 'lifecycle: draft'",
            "range_start_byte": 12,
            "range_end_byte": 48,
            "native_skill_name": "visual-spec",
            "native_skill_version": "0.1.0",
            "note": "replace markdown tip prose with <Callout />"
        });
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Patchy",
                body: "lifecycle: <Callout>draft</Callout>",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: Some("mdx"),
                event_metadata: Some(&metadata),
            },
        )
        .await
        .unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        let head = revisions.last().expect("head revision");
        assert_eq!(head.seq, 2);
        let stored = head
            .event_metadata
            .as_deref()
            .expect("head revision must carry event_metadata for a targeted patch");
        let parsed: serde_json::Value = serde_json::from_str(stored)
            .expect("event_metadata must be a valid JSON document on the read path");
        assert_eq!(parsed, metadata);
        // Stable field-by-field contract: every key the design promises is
        // present and round-trips byte-for-byte. This is what the
        // `proposal_show`/revision model surfaces to UI consumers.
        assert_eq!(parsed["change_kind"], "targeted_block_patch");
        assert_eq!(parsed["block_id"], "callout-tip");
        assert_eq!(parsed["native_skill_name"], "visual-spec");
        assert_eq!(parsed["native_skill_version"], "0.1.0");
        assert_eq!(parsed["range_start_byte"], 12);
        assert_eq!(parsed["range_end_byte"], 48);
    }

    /// Two successive material updates — each carrying distinct metadata —
    /// must each land on their own revision row, with `latest_revision_seq`
    /// advancing once per patch. This is the per-patch attribution contract
    /// the design pins (one revision per targeted block-patch, not a
    /// monolithic body rewrite).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_targeted_patches_persist_two_distinct_revisions() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Multi Patch")).await.unwrap();

        let first = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "block_id": "callout-tip",
            "native_skill_name": "visual-spec",
            "native_skill_version": "0.1.0",
        });
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Multi Patch",
                body: "patch-1 body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: Some("mdx"),
                event_metadata: Some(&first),
            },
        )
        .await
        .unwrap();

        let second = serde_json::json!({
            "change_kind": "targeted_block_patch",
            "block_id": "metric-tile",
            "selector": "section: '## Metrics'",
            "native_skill_name": "visual-spec",
            "native_skill_version": "0.1.0",
        });
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: "Multi Patch",
                body: "patch-1 + patch-2 body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: Some("mdx"),
                event_metadata: Some(&second),
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&p.id).await.unwrap().expect("proposal row");
        assert_eq!(updated.latest_revision_seq, 3, "seed + two patches = 3");

        let revisions = repo.revisions(&p.id).await.unwrap();
        assert_eq!(revisions.len(), 3);
        assert!(revisions[0].event_metadata.is_none(), "seed stays NULL");
        let r1: serde_json::Value = serde_json::from_str(
            revisions[1]
                .event_metadata
                .as_deref()
                .expect("first patch metadata"),
        )
        .unwrap();
        assert_eq!(r1["block_id"], "callout-tip");
        let r2: serde_json::Value = serde_json::from_str(
            revisions[2]
                .event_metadata
                .as_deref()
                .expect("second patch metadata"),
        )
        .unwrap();
        assert_eq!(r2["block_id"], "metric-tile");
        assert_eq!(r2["selector"], "section: '## Metrics'");
    }

    /// `dangling_refinement_proposal_ids` reports a proposal exactly while it
    /// has more `refinement_start` than `refinement_stop` lifecycle rows — the
    /// signal startup recovery uses to reconcile refinements lost across a
    /// restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dangling_refinement_ids_track_unmatched_start() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Dangling Refinement")).await.unwrap();

        // No refinement lifecycle yet → not dangling.
        assert!(
            repo.dangling_refinement_proposal_ids()
                .await
                .unwrap()
                .is_empty()
        );

        // A start with no matching stop → dangling.
        repo.record_refinement_lifecycle(&p.id, "refinement_start", None)
            .await
            .unwrap();
        assert_eq!(
            repo.dangling_refinement_proposal_ids().await.unwrap(),
            vec![p.id.clone()]
        );

        // An awaiting_review event does not balance the start → still dangling.
        repo.record_refinement_lifecycle(&p.id, "refinement_awaiting_review", None)
            .await
            .unwrap();
        assert_eq!(
            repo.dangling_refinement_proposal_ids().await.unwrap(),
            vec![p.id.clone()]
        );

        // A matching stop → balanced → no longer dangling.
        repo.record_refinement_lifecycle(&p.id, "refinement_stop", None)
            .await
            .unwrap();
        assert!(
            repo.dangling_refinement_proposal_ids()
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Status-only updates and the `status_change` audit events they emit
    /// must keep `event_metadata` NULL. The audit trail of lifecycle
    /// transitions is not authoring metadata and should not be conflated
    /// with the targeted-patch contract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_only_event_keeps_event_metadata_null() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Status Only")).await.unwrap();
        // Move the proposal to `done` (status-only path that triggers a
        // `status_change` audit row in addition to the create seed).
        repo.update(
            &p.id,
            ProposalUpdateInput {
                title: &p.title,
                body: &p.body,
                acceptance_criteria: &p.acceptance_criteria,
                status: "done",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();
        let revisions = repo.revisions(&p.id).await.unwrap();
        // seed (spec_revision) + status_change audit
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[1].event_kind, "status_change");
        assert!(
            revisions[1].event_metadata.is_none(),
            "status_change rows must leave event_metadata NULL"
        );
    }

    // ── Debate trail tests ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_append_and_list_ordered() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Trail")).await.unwrap();
        captured.lock().unwrap().clear();

        // Append three entries in mixed order; list should return round then created_at.
        let obj = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "objection",
                body: "too broad",
                blocking: true,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
            })
            .await
            .unwrap();
        assert_eq!(obj.kind, "objection");
        assert!(obj.blocking);
        assert_eq!(obj.round, 1);
        assert!(obj.resolved_at.is_none());
        assert!(obj.reopened_at.is_none());

        let reb = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "rebuttal",
                body: "scope is fine because...",
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
            })
            .await
            .unwrap();
        assert_eq!(reb.kind, "rebuttal");

        let verdict = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "verdict",
                body: "narrow scope to X",
                blocking: false,
                agent_role: "judge",
                author_kind: "agent",
                author_model: Some("claude-opus-4-8"),
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
            })
            .await
            .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 3);
        // Ordered by round then created_at; ids are UUIDv7 so created_at ordering
        // is deterministic within the same millisecond.
        assert_eq!(trail[0].id, obj.id);
        assert_eq!(trail[1].id, reb.id);
        assert_eq!(trail[2].id, verdict.id);

        // get by id works
        let fetched = repo.get_debate_trail_entry(&obj.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, obj.id);

        // events fired for each append
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|e| e.entity_type == "proposal_debate_trail")
        );
        assert!(events.iter().all(|e| e.action == "created"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_isolation_by_proposal() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p1 = repo.create(create_input("One")).await.unwrap();
        let p2 = repo.create(create_input("Two")).await.unwrap();

        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p1.id,
            kind: "objection",
            body: "obj-1",
            blocking: false,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
        })
        .await
        .unwrap();

        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p2.id,
            kind: "rebuttal",
            body: "reb-2",
            blocking: false,
            agent_role: "advocate",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
        })
        .await
        .unwrap();

        let trail1 = repo.debate_trail(&p1.id).await.unwrap();
        let trail2 = repo.debate_trail(&p2.id).await.unwrap();
        assert_eq!(trail1.len(), 1);
        assert_eq!(trail1[0].body, "obj-1");
        assert_eq!(trail2.len(), 1);
        assert_eq!(trail2[0].body, "reb-2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_resolve_and_reopen() {
        let (bus, captured) = capturing_bus();
        let repo = ProposalRepository::new(test_db(), bus);
        let p = repo.create(create_input("Resolve")).await.unwrap();
        captured.lock().unwrap().clear();

        let entry = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "objection",
                body: "blocking issue",
                blocking: true,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
            })
            .await
            .unwrap();

        // Resolve it.
        let resolved = repo.resolve_debate_trail_entry(&entry.id).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        assert!(resolved.reopened_at.is_none());

        // Reopen it.
        let reopened = repo.reopen_debate_trail_entry(&entry.id).await.unwrap();
        assert!(reopened.resolved_at.is_some());
        assert!(reopened.reopened_at.is_some());

        // Re-resolve clears reopen state.
        let re_resolved = repo.resolve_debate_trail_entry(&entry.id).await.unwrap();
        assert!(re_resolved.resolved_at.is_some());
        assert!(re_resolved.reopened_at.is_none());
        assert!(re_resolved.reopened_by_user_id.is_none());

        let events = captured.lock().unwrap();
        // 1 created + 3 updates = 4 events
        assert_eq!(events.len(), 4);
        assert!(
            events
                .iter()
                .all(|e| e.entity_type == "proposal_debate_trail")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_invalid_kind_rejected() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Invalid")).await.unwrap();

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &p.id,
                kind: "comment",
                body: "nope",
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid debate trail kind"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_proposal_must_exist() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());

        let err = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: "nonexistent-id",
                kind: "objection",
                body: "nope",
                blocking: false,
                agent_role: "adversary",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: 1,
                round: 1,
            })
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("proposal not found"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_trail_multiround_ordering() {
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Rounds")).await.unwrap();

        // Round 1
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "objection",
            body: "r1-obj",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
        })
        .await
        .unwrap();

        // Round 2
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "rebuttal",
            body: "r2-reb",
            blocking: false,
            agent_role: "advocate",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 2,
            round: 2,
        })
        .await
        .unwrap();

        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[0].round, 1);
        assert_eq!(trail[0].body, "r1-obj");
        assert_eq!(trail[1].round, 2);
        assert_eq!(trail[1].body, "r2-reb");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn existing_feedback_crud_unaffected_by_debate_trail() {
        // Verify that adding debate-trail entries does not interfere with
        // existing proposal_feedback operations.
        let repo = ProposalRepository::new(test_db(), EventBus::noop());
        let p = repo.create(create_input("Both")).await.unwrap();

        // Add a feedback entry.
        let fb = repo
            .add_feedback(ProposalFeedbackCreateInput {
                proposal_id: &p.id,
                parent_id: None,
                author_kind: "user",
                author_model: None,
                body: "human comment",
            })
            .await
            .unwrap();
        assert!(fb.resolved_at.is_none());

        // Add a debate trail entry.
        repo.add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &p.id,
            kind: "objection",
            body: "ai objection",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
        })
        .await
        .unwrap();

        // Feedback still works independently.
        let feedbacks = repo.feedback(&p.id).await.unwrap();
        assert_eq!(feedbacks.len(), 1);
        assert_eq!(feedbacks[0].body, "human comment");

        let resolved = repo.set_feedback_resolved(&fb.id, Some(2)).await.unwrap();
        assert!(resolved.resolved_at.is_some());
        assert_eq!(resolved.resolved_revision_seq, Some(2));

        // Debate trail is still separate.
        let trail = repo.debate_trail(&p.id).await.unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].body, "ai objection");
    }
}
