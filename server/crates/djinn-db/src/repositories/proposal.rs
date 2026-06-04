use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{Proposal, ProposalFeedback, ProposalTarget};

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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at
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
            "INSERT INTO proposals (id, short_id, title, body, acceptance_criteria, status, author_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
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
        sqlx::query!(
            r#"UPDATE proposals SET title = $1, body = $2, acceptance_criteria = $3, status = $4,
                    superseded_by = $5,
                    closed_at = CASE WHEN $6 IN ('done', 'rejected', 'archived', 'superseded')
                        THEN COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
                        ELSE NULL END,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $7"#,
            input.title,
            input.body,
            acceptance_criteria,
            input.status,
            input.superseded_by,
            input.status,
            id
        )
        .execute(self.db.pool())
        .await?;
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
                    body, target_section, status, created_at, updated_at
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
                    body, target_section, status, created_at, updated_at
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
                (id, proposal_id, parent_id, author_kind, author_user_id, author_model, body, target_section, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            id,
            input.proposal_id,
            input.parent_id,
            input.author_kind,
            author_user_id,
            input.author_model,
            input.body,
            input.target_section,
            input.status
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
                    status, author_user_id, superseded_by, created_at, updated_at, closed_at
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
        clauses.push(format!("(title LIKE {ph_a} OR body LIKE {ph_b})"));
        let pattern = format!("%{t}%");
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
}
