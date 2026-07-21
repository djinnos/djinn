// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{Epic, EpicEventPayload};

use crate::database::Database;
use crate::repositories::task::{DispositionScope, TaskRepository, apply_parent_disposition_tx};
use crate::{Error, Result};

// Inlined EPIC_COLS projection for each `query_as!(Epic, ...)` call site.
// `query_as!` requires a string-literal SQL argument; concat!()-produced
// literals don't satisfy it (verified during batch 4 on agent.rs).  Each
// caller therefore passes the full SELECT body as a raw string literal.

// ── Query / result types ─────────────────────────────────────────────────────

/// Aggregate child-task counts for an epic.
pub struct EpicTaskCounts {
    pub task_count: i64,
    pub open_count: i64,
    pub in_progress_count: i64,
    pub closed_count: i64,
}

/// Minimal epic reference returned by epic-blocker listing queries.
#[derive(Debug, sqlx::FromRow)]
pub struct EpicBlockerRef {
    pub epic_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

/// Filters and pagination for [`EpicRepository::list_filtered`].
pub struct EpicListQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub text: Option<String>,
    pub sort: String,
    pub limit: i64,
    pub offset: i64,
}

impl Default for EpicListQuery {
    fn default() -> Self {
        Self {
            status: None,
            project_id: None,
            text: None,
            sort: "created".to_owned(),
            limit: 25,
            offset: 0,
        }
    }
}

pub struct EpicListResult {
    pub epics: Vec<Epic>,
    pub total_count: i64,
}

/// Filters for [`EpicRepository::count_grouped`].
pub struct EpicCountQuery {
    pub project_id: Option<String>,
    pub status: Option<String>,
    pub group_by: Option<String>,
}

#[derive(Clone, Debug)]
enum SqlParam {
    Text(String),
}

pub struct EpicCreateInput<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub emoji: &'a str,
    pub color: &'a str,
    pub owner: &'a str,
    pub memory_refs: Option<&'a str>,
    /// Epic status: "open" (default) or "closed".
    pub status: Option<&'a str>,
    /// ADR-051 Epic C — if `None`, defaults to `true` (existing behaviour).
    /// When `false`, the coordinator skips the epic_created breakdown
    /// auto-dispatch.
    pub auto_breakdown: Option<bool>,
    /// ADR-051 Epic C — slug of the accepted ADR that spawned this epic.
    pub originating_adr_id: Option<&'a str>,
    /// Epic-level blocked_by references (UUIDs or short_ids) to wire at
    /// creation time, so the `epic_created` event is only emitted AFTER
    /// blocker edges exist in the DB.  Resolved inside `create_for_project`.
    pub blocked_by: Option<&'a [&'a str]>,
}

pub type EpicUpdateInput<'a> = EpicCreateInput<'a>;

pub struct EpicRepository {
    db: Database,
    events: EventBus,
}

impl EpicRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Hydrate the proposal swimlane labels (short_id/title/status/build
    /// owner) for an epic's event payload, mirroring the `epic_list`
    /// enrichment so live SSE payloads match snapshot rows. Fail-open: a
    /// lookup failure logs and falls back to a bare payload — the event must
    /// still go out so live boards hear about the change.
    async fn event_payload<'a>(&self, epic: &'a Epic) -> EpicEventPayload<'a> {
        let Some(proposal_id) = epic.proposal_id.clone() else {
            return EpicEventPayload::bare(epic);
        };
        let proposals = crate::repositories::proposal::ProposalRepository::new(
            self.db.clone(),
            self.events.clone(),
        );
        match proposals.refs_by_ids(&[proposal_id]).await {
            Ok(refs) => match refs.into_iter().next() {
                Some(r) => EpicEventPayload {
                    epic,
                    proposal_short_id: Some(r.short_id),
                    proposal_title: Some(r.title),
                    proposal_status: Some(r.status),
                    proposal_build_owner_user_id: r.build_owner_user_id,
                },
                None => EpicEventPayload::bare(epic),
            },
            Err(e) => {
                tracing::warn!(
                    epic_id = %epic.id,
                    error = %e,
                    "failed to hydrate proposal labels for epic event; sending bare payload"
                );
                EpicEventPayload::bare(epic)
            }
        }
    }

    /// Emit `epic.created` with hydrated proposal swimlane labels.
    pub async fn emit_created(&self, epic: &Epic) {
        self.events.send(DjinnEventEnvelope::epic_created(
            &self.event_payload(epic).await,
        ));
    }

    /// Emit `epic.updated` with hydrated proposal swimlane labels.
    pub async fn emit_updated(&self, epic: &Epic) {
        self.events.send(DjinnEventEnvelope::epic_updated(
            &self.event_payload(epic).await,
        ));
    }

    /// Close a set of epics inside an existing lifecycle transaction.
    pub async fn close_scoped_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        epic_ids: &[String],
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE epics SET status = 'closed',
                    closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = ANY($1)"#,
        )
        .bind(epic_ids)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics ORDER BY created_at"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List epics for a specific project, ordered by creation time.
    pub async fn list_for_project(&self, project_id: &str) -> Result<Vec<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Epic>(
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status, owner, created_at, updated_at, closed_at,
                    memory_refs::text, auto_breakdown,
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE project_id = $1 ORDER BY created_at"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_short_id(&self, short_id: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE short_id = $1"#,
            short_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn create(
        &self,
        title: &str,
        description: &str,
        emoji: &str,
        color: &str,
        owner: &str,
        memory_refs: Option<&str>,
    ) -> Result<Epic> {
        let project_id = self.ensure_default_project_id().await?;
        self.create_for_project(
            &project_id,
            EpicCreateInput {
                title,
                description,
                emoji,
                color,
                owner,
                memory_refs,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
    }

    pub async fn create_for_project(
        &self,
        project_id: &str,
        input: EpicCreateInput<'_>,
    ) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = self.generate_short_id(&id).await?;
        let status = input.status.unwrap_or("open");
        let auto_breakdown = input.auto_breakdown.unwrap_or(true);
        let memory_refs_str = input.memory_refs.unwrap_or("[]");
        let memory_refs: serde_json::Value = serde_json::from_str(memory_refs_str)
            .map_err(|e| Error::InvalidData(format!("invalid json for epics.memory_refs: {e}")))?;
        // Phase 3B: stamp `created_by_user_id` from the task-local set at
        // the MCP dispatch root. `None` when no user context is in scope.
        let created_by_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, status, owner, memory_refs, auto_breakdown, originating_adr_id, created_by_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            id,
            project_id,
            short_id,
            input.title,
            input.description,
            input.emoji,
            input.color,
            status,
            input.owner,
            memory_refs,
            auto_breakdown,
            input.originating_adr_id,
            created_by_user_id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        // Wire epic-level blocked_by edges before emitting the event, so
        // the coordinator's has_unresolved_blockers gate sees them at check
        // time.  Reuses update_blockers_atomic which has cycle detection and
        // ON CONFLICT DO NOTHING.
        if let Some(blocker_refs) = input.blocked_by {
            let mut blocker_ids = Vec::new();
            for blocker_ref in blocker_refs {
                if let Some(blocker) = self.resolve(blocker_ref).await? {
                    blocker_ids.push(blocker.id);
                }
            }
            if !blocker_ids.is_empty() {
                self.update_blockers_atomic(&id, &blocker_ids, &[]).await?;
            }
        }

        self.emit_created(&epic).await;
        Ok(epic)
    }

    pub async fn update(&self, id: &str, input: EpicUpdateInput<'_>) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let status = input.status.unwrap_or("open");
        let memory_refs_str = input.memory_refs.unwrap_or("[]");
        let memory_refs: serde_json::Value = serde_json::from_str(memory_refs_str)
            .map_err(|e| Error::InvalidData(format!("invalid json for epics.memory_refs: {e}")))?;
        sqlx::query!(
            r#"UPDATE epics SET title = $1, description = $2, emoji = $3,
                    color = $4, status = $5, owner = $6, memory_refs = $7,
                    closed_at = CASE WHEN $8 = 'closed' THEN COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')) ELSE NULL END,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $9"#,
            input.title,
            input.description,
            input.emoji,
            input.color,
            status,
            input.owner,
            memory_refs,
            status,
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.emit_updated(&epic).await;
        Ok(epic)
    }

    pub async fn close(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;

        // The shared primitive locks, reclassifies, mutates, and audits child
        // dispositions in this same transaction. Proposal terminal paths use
        // the identical matrix through their own canonical scopes.
        let scope = DispositionScope::for_epic_close(id);
        let mut tx = self.db.pool().begin().await?;
        let disposition_plan = apply_parent_disposition_tx(&mut tx, &scope).await?;

        // Close the epic itself within the same transaction.
        sqlx::query(
            r#"UPDATE epics SET status = 'closed',
                    closed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let epic: Epic = sqlx::query_as::<_, Epic>(
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status, owner, created_at, updated_at, closed_at,
                    memory_refs::text, auto_breakdown,
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        // ── Post-commit events ─────────────────────────────────────────────
        let task_repo = TaskRepository::new(self.db.clone(), self.events.clone());
        for finding in disposition_plan
            .findings
            .iter()
            .filter(|finding| finding.disposition.applies_change())
        {
            if let Some(task) = task_repo.get(&finding.task_id).await? {
                self.events
                    .send(DjinnEventEnvelope::task_updated(&task, false));
            }
        }

        self.emit_updated(&epic).await;
        // Re-drive wave-1 for any epics that were blocked by this one and are
        // now fully unblocked (mirror of task `emit_unblocked_tasks`).
        self.emit_unblocked_epics(id).await?;
        Ok(epic)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("DELETE FROM epics WHERE id = $1", id)
            .execute(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::epic_deleted(id));
        Ok(())
    }

    /// Set the epic's creator. Used by proposal graduation to attribute the
    /// build (and therefore commits, via the task-creator → commit-author
    /// chain) to the chosen build owner.
    pub async fn set_created_by_user_id(&self, id: &str, user_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE epics SET created_by_user_id = $1 WHERE id = $2",
            user_id,
            id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Replace the `memory_refs` JSON array on an epic.
    pub async fn update_memory_refs(&self, id: &str, memory_refs_json: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let memory_refs: serde_json::Value = serde_json::from_str(memory_refs_json)
            .map_err(|e| Error::InvalidData(format!("invalid json for epics.memory_refs: {e}")))?;
        sqlx::query!(
            r#"UPDATE epics SET memory_refs = $1,
                updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            memory_refs,
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.emit_updated(&epic).await;
        Ok(epic)
    }

    // ── Read sources (read-only multi-repo) ──────────────────────────────────

    /// List the project IDs this epic is allowed to READ. Writes stay
    /// pinned to the epic's own `project_id`; these are additional
    /// read-only sources (e.g. the legacy repo in an A→B migration epic).
    /// Ordered by when each grant was added.
    pub async fn read_sources(&self, epic_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar!(
            "SELECT project_id FROM epic_read_sources WHERE epic_id = $1 ORDER BY created_at",
            epic_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Grant this epic read access to `project_id`. Idempotent. The
    /// `project_id` must reference a registered project — the FK enforces
    /// this, so a bad ref surfaces as a DB error; callers should resolve
    /// the project first to return a friendly message.
    pub async fn add_read_source(&self, epic_id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "INSERT INTO epic_read_sources (epic_id, project_id) VALUES ($1, $2)
             ON CONFLICT (epic_id, project_id) DO NOTHING",
            epic_id,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        if let Some(epic) = self.get(epic_id).await? {
            self.emit_updated(&epic).await;
        }
        Ok(())
    }

    /// Revoke this epic's read access to `project_id`. No-op if absent.
    pub async fn remove_read_source(&self, epic_id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM epic_read_sources WHERE epic_id = $1 AND project_id = $2",
            epic_id,
            project_id
        )
        .execute(self.db.pool())
        .await?;
        if let Some(epic) = self.get(epic_id).await? {
            self.emit_updated(&epic).await;
        }
        Ok(())
    }

    /// Resolve the effective read set for a task: the read sources of its
    /// epic, or empty when the task has no epic. The task's own project is
    /// the write target and is intentionally NOT included here.
    pub async fn read_sources_for_task(&self, task_epic_id: Option<&str>) -> Result<Vec<String>> {
        match task_epic_id {
            Some(epic_id) => self.read_sources(epic_id).await,
            None => Ok(Vec::new()),
        }
    }

    // ── Epic dependencies (mirror of task `blockers`) ────────────────────────

    /// Add an epic-blocker relationship: `epic_id` is blocked by `blocking_id`.
    /// Rejects self-loops and cycles (recursive CTE over the blocking graph).
    /// Idempotent on duplicate edges.
    pub async fn add_blocker(&self, epic_id: &str, blocking_id: &str) -> Result<()> {
        if epic_id == blocking_id {
            return Err(Error::Internal("epic cannot block itself".into()));
        }
        self.db.ensure_initialized().await?;

        let epic_id_owned = epic_id.to_owned();
        let blocking_id_owned = blocking_id.to_owned();
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let epic_id = epic_id_owned.clone();
                let blocking_id = blocking_id_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    let would_cycle = sqlx::query_scalar!(
                        r#"WITH RECURSIVE reach(id) AS (
                             SELECT epic_id FROM epic_blockers WHERE blocking_epic_id = $1
                             UNION
                             SELECT b.epic_id FROM epic_blockers b JOIN reach r ON b.blocking_epic_id = r.id
                         )
                         SELECT EXISTS(SELECT 1 FROM reach WHERE id = $2) AS "exists!: bool""#,
                        epic_id,
                        blocking_id
                    )
                    .fetch_one(&mut *tx)
                    .await?;
                    if would_cycle {
                        return Err(Error::Internal(
                            "would create circular epic blocker dependency".into(),
                        ));
                    }
                    let result = sqlx::query!(
                        "INSERT INTO epic_blockers (epic_id, blocking_epic_id) VALUES ($1, $2)",
                        epic_id,
                        blocking_id
                    )
                    .execute(&mut *tx)
                    .await;
                    match result {
                        Ok(_) => {}
                        Err(sqlx::Error::Database(ref e)) if e.is_unique_violation() => {}
                        Err(e) => {
                            return Err(Error::Internal(format!(
                                "failed to add epic blocker {blocking_id} → {epic_id}: {e}"
                            )));
                        }
                    }
                    tx.commit().await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;

        if let Some(epic) = self.get(epic_id).await? {
            self.emit_updated(&epic).await;
        }
        if let Some(epic) = self.get(blocking_id).await? {
            self.emit_updated(&epic).await;
        }
        Ok(())
    }

    /// Remove an epic-blocker relationship. Idempotent.
    pub async fn remove_blocker(&self, epic_id: &str, blocking_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "DELETE FROM epic_blockers WHERE epic_id = $1 AND blocking_epic_id = $2",
            epic_id,
            blocking_id
        )
        .execute(self.db.pool())
        .await?;

        if let Some(epic) = self.get(epic_id).await? {
            self.emit_updated(&epic).await;
        }
        if let Some(epic) = self.get(blocking_id).await? {
            self.emit_updated(&epic).await;
        }
        Ok(())
    }

    /// Atomically apply a batch of epic-blocker additions and removals in a
    /// single transaction (mirror of [`TaskRepository::update_blockers_atomic`]).
    pub async fn update_blockers_atomic(
        &self,
        epic_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let epic_id_owned = epic_id.to_owned();
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let epic_id = epic_id_owned.clone();
                async move {
                    let mut tx = self.db.pool().begin().await?;
                    for blocking_id in remove {
                        sqlx::query!(
                            "DELETE FROM epic_blockers WHERE epic_id = $1 AND blocking_epic_id = $2",
                            epic_id,
                            blocking_id
                        )
                        .execute(&mut *tx)
                        .await?;
                    }
                    for blocking_id in add {
                        if &epic_id == blocking_id {
                            return Err(Error::Internal("epic cannot block itself".into()));
                        }
                        let would_cycle = sqlx::query_scalar!(
                            r#"WITH RECURSIVE reach(id) AS (
                                 SELECT epic_id FROM epic_blockers WHERE blocking_epic_id = $1
                                 UNION
                                 SELECT b.epic_id FROM epic_blockers b JOIN reach r ON b.blocking_epic_id = r.id
                             )
                             SELECT EXISTS(SELECT 1 FROM reach WHERE id = $2) AS "exists!: bool""#,
                            epic_id,
                            blocking_id
                        )
                        .fetch_one(&mut *tx)
                        .await?;
                        if would_cycle {
                            return Err(Error::Internal(
                                "would create circular epic blocker dependency".into(),
                            ));
                        }
                        let result = sqlx::query!(
                            "INSERT INTO epic_blockers (epic_id, blocking_epic_id) VALUES ($1, $2)",
                            epic_id,
                            blocking_id
                        )
                        .execute(&mut *tx)
                        .await;
                        match result {
                            Ok(_) => {}
                            Err(sqlx::Error::Database(ref e)) if e.is_unique_violation() => {}
                            Err(e) => {
                                return Err(Error::Internal(format!(
                                    "failed to add epic blocker {blocking_id} → {epic_id}: {e}"
                                )));
                            }
                        }
                    }
                    tx.commit().await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;

        let mut notified = std::collections::HashSet::new();
        notified.insert(epic_id.to_owned());
        for id in add.iter().chain(remove.iter()) {
            notified.insert(id.clone());
        }
        for id in &notified {
            if let Some(epic) = self.get(id).await? {
                self.emit_updated(&epic).await;
            }
        }
        Ok(())
    }

    /// List epics blocking `epic_id`.
    pub async fn list_blockers(&self, epic_id: &str) -> Result<Vec<EpicBlockerRef>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            EpicBlockerRef,
            r#"SELECT e.id AS epic_id, e.short_id, e.title, e.status AS "status!"
             FROM epic_blockers b
             JOIN epics e ON e.id = b.blocking_epic_id
             WHERE b.epic_id = $1
             ORDER BY e.created_at"#,
            epic_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List epics blocked BY `blocking_epic_id`.
    pub async fn list_blocked_by(&self, blocking_epic_id: &str) -> Result<Vec<EpicBlockerRef>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            EpicBlockerRef,
            r#"SELECT e.id AS epic_id, e.short_id, e.title, e.status AS "status!"
             FROM epic_blockers b
             JOIN epics e ON e.id = b.epic_id
             WHERE b.blocking_epic_id = $1
             ORDER BY e.created_at"#,
            blocking_epic_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// True iff `epic_id` has at least one blocking epic that is not yet closed.
    /// Used by the coordinator to gate wave-1 auto-dispatch.
    pub async fn has_unresolved_blockers(&self, epic_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar!(
            r#"SELECT EXISTS(
                 SELECT 1 FROM epic_blockers b
                 JOIN epics be ON be.id = b.blocking_epic_id
                 WHERE b.epic_id = $1 AND be.status != 'closed'
             ) AS "exists!: bool""#,
            epic_id
        )
        .fetch_one(self.db.pool())
        .await?)
    }

    /// Emit `epic.updated` for epics that were blocked by `closed_epic_id` and
    /// are now fully unblocked (all blockers closed) and still open. This
    /// re-drives the coordinator's wave-1 path for the dependent epics.
    pub async fn emit_unblocked_epics(&self, closed_epic_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let unblocked: Vec<Epic> = sqlx::query_as!(
            Epic,
            r#"SELECT e.id, e.project_id, e.short_id, e.title, e.description, e.emoji, e.color,
                    e.status AS "status!", e.owner, e.created_at, e.updated_at, e.closed_at,
                    e.memory_refs::text AS "memory_refs!", e.auto_breakdown AS "auto_breakdown!: bool",
                    e.originating_adr_id, e.created_by_user_id, e.proposal_id
             FROM epic_blockers b
             JOIN epics e ON e.id = b.epic_id
             WHERE b.blocking_epic_id = $1
               AND e.status = 'open'
               AND NOT EXISTS (
                   SELECT 1 FROM epic_blockers b2
                   JOIN epics be ON be.id = b2.blocking_epic_id
                   WHERE b2.epic_id = e.id AND be.status != 'closed'
               )"#,
            closed_epic_id
        )
        .fetch_all(self.db.pool())
        .await?;

        for epic in unblocked {
            self.emit_updated(&epic).await;
        }
        Ok(())
    }

    // ── New methods (ADR-003) ────────────────────────────────────────────────

    /// Resolve an epic by UUID or short_id.
    pub async fn resolve(&self, id_or_short: &str) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1 OR short_id = $2"#,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve an epic by UUID or short_id constrained to a project.
    pub async fn resolve_in_project(
        &self,
        project_id: &str,
        id_or_short: &str,
    ) -> Result<Option<Epic>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE project_id = $1 AND (id = $2 OR short_id = $3)"#,
            project_id,
            id_or_short,
            id_or_short
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Force-set `status` on an epic without transition validation.
    ///
    /// Unlike [`Self::close`] / [`Self::reopen`], this skips the current-
    /// status precondition. Intended for tests that seed a specific status
    /// (e.g. promoting a `drafting` epic straight to `open`). Production
    /// callers should prefer the transition-checked methods.
    pub async fn set_status_raw(&self, id: &str, status: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE epics SET status = $1,
                    closed_at = CASE WHEN $2 = 'closed' THEN COALESCE(closed_at, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')) ELSE NULL END,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
            status,
            status,
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.emit_updated(&epic).await;
        Ok(epic)
    }

    /// Reopen a closed epic: set status=open, clear closed_at.
    pub async fn reopen(&self, id: &str) -> Result<Epic> {
        self.db.ensure_initialized().await?;
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("epic not found: {id}")))?;
        if current.status != "closed" {
            return Err(Error::InvalidTransition(format!(
                "epic must be closed to reopen (current: {})",
                current.status
            )));
        }
        sqlx::query!(
            r#"UPDATE epics SET status = 'open',
                    closed_at = NULL,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            id
        )
        .execute(self.db.pool())
        .await?;
        let epic: Epic = sqlx::query_as!(
            Epic,
            r#"SELECT id, project_id, short_id, title, description, emoji, color,
                    status AS "status!", owner, created_at, updated_at, closed_at,
                    memory_refs::text AS "memory_refs!", auto_breakdown AS "auto_breakdown!: bool",
                    originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.emit_updated(&epic).await;
        Ok(epic)
    }

    /// Aggregate child-task counts for an epic.
    pub async fn task_counts(&self, epic_id: &str) -> Result<EpicTaskCounts> {
        self.db.ensure_initialized().await?;
        // MySQL/Dolt returns SUM(...) as DECIMAL; casting to SIGNED keeps the
        // value decodeable as i64 via sqlx (otherwise large DECIMAL round-trip
        // blows up to sign-extended 2^62 on Dolt).
        let row = sqlx::query!(
            r#"SELECT
                COUNT(*) AS "task_count!: i64",
                CAST(COALESCE(SUM(CASE WHEN status = 'open' THEN 1 ELSE 0 END), 0) AS BIGINT) AS "open_count!: i64",
                CAST(COALESCE(SUM(CASE WHEN status = 'in_progress' THEN 1 ELSE 0 END), 0) AS BIGINT) AS "in_progress_count!: i64",
                CAST(COALESCE(SUM(CASE WHEN status = 'closed' THEN 1 ELSE 0 END), 0) AS BIGINT) AS "closed_count!: i64"
             FROM tasks WHERE epic_id = $1"#,
            epic_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(EpicTaskCounts {
            task_count: row.task_count,
            open_count: row.open_count,
            in_progress_count: row.in_progress_count,
            closed_count: row.closed_count,
        })
    }

    /// Count child tasks then CASCADE-delete the epic. Returns the child task count.
    pub async fn delete_with_count(&self, id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM tasks WHERE epic_id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;
        self.delete(id).await?;
        Ok(count)
    }

    /// List epics with optional filters, sorting, and pagination.
    pub async fn list_filtered(&self, query: EpicListQuery) -> Result<EpicListResult> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) =
            epic_build_where(&query.project_id, &query.status, &query.text, 0);
        let order_sql = epic_sort_to_sql(&query.sort);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM epics WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            total_q = total_q.bind(s.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let limit_ph = format!("${}", params.len() + 1);
        let offset_ph = format!("${}", params.len() + 2);
        // NOTE: dynamic SQL (WHERE + ORDER clauses built from optional filters; uses inlined EPIC_COLS projection) — compile-time check not possible
        let sql = format!(
            r#"SELECT id, project_id, short_id, title, description, emoji, color, status,
                    owner, created_at, updated_at, closed_at, memory_refs::text AS memory_refs,
                    auto_breakdown, originating_adr_id, created_by_user_id, proposal_id
             FROM epics WHERE {where_sql} ORDER BY {order_sql} LIMIT {limit_ph} OFFSET {offset_ph}"#
        );
        let mut epic_q = sqlx::query_as::<_, Epic>(&sql);
        for p in &params {
            let SqlParam::Text(s) = p;
            epic_q = epic_q.bind(s.clone());
        }
        let epics = epic_q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

        Ok(EpicListResult {
            epics,
            total_count: total,
        })
    }

    /// Count epics with optional group_by.
    pub async fn count_grouped(&self, query: EpicCountQuery) -> Result<serde_json::Value> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = epic_build_where(&query.project_id, &query.status, &None, 0);

        match query.group_by.as_deref() {
            Some("status") => {
                // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
                let sql = format!(
                    "SELECT status, COUNT(*) FROM epics WHERE {where_sql}
                     GROUP BY status ORDER BY COUNT(*) DESC, status"
                );
                let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
                for p in &params {
                    let SqlParam::Text(s) = p;
                    q = q.bind(s.clone());
                }
                let groups = q
                    .fetch_all(self.db.pool())
                    .await?
                    .into_iter()
                    .map(|(key, count)| serde_json::json!({"key": key, "count": count}))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "groups": groups }))
            }
            Some(other) => Err(Error::InvalidData(format!("unknown group_by: {other}"))),
            None => {
                // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
                let sql = format!("SELECT COUNT(*) FROM epics WHERE {where_sql}");
                let mut q = sqlx::query_scalar::<_, i64>(&sql);
                for p in &params {
                    let SqlParam::Text(s) = p;
                    q = q.bind(s.clone());
                }
                let total = q.fetch_one(self.db.pool()).await?;
                Ok(serde_json::json!({ "total_count": total }))
            }
        }
    }

    /// Generate a unique 4-char base36 short ID for the epics table.
    async fn generate_short_id(&self, seed_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;
        let seed = uuid::Uuid::parse_str(seed_id).map_err(|e| Error::InvalidData(e.to_string()))?;
        let candidate = short_id_from_uuid(&seed);
        if !short_id_exists(self.db.pool(), "epics", &candidate).await? {
            return Ok(candidate);
        }
        for _ in 0..16 {
            let candidate = short_id_from_uuid(&uuid::Uuid::now_v7());
            if !short_id_exists(self.db.pool(), "epics", &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(Error::InvalidData(
            "short_id collision after 16 retries".into(),
        ))
    }

    async fn ensure_default_project_id(&self) -> Result<String> {
        self.db.ensure_initialized().await?;
        if let Some(id) = sqlx::query_scalar!("SELECT id FROM projects ORDER BY created_at LIMIT 1")
            .fetch_optional(self.db.pool())
            .await?
        {
            return Ok(id);
        }

        let id = uuid::Uuid::now_v7().to_string();
        let owner = "test";
        const MAX_GITHUB_REPO_LEN: usize = 36;
        // `projects.github_repo` is varchar(36) in the real Postgres schema.
        // Keep the synthesized default project slug below that cap while still
        // deriving it from the UUID so concurrent tests do not collide.
        let compact_id = id.replace('-', "");
        let repo_slug = format!("default-{}", &compact_id[..28]);
        debug_assert!(
            repo_slug.len() <= MAX_GITHUB_REPO_LEN,
            "projects.github_repo is varchar({MAX_GITHUB_REPO_LEN})"
        );

        sqlx::query!(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
            id,
            "default",
            owner,
            repo_slug,
        )
        .execute(self.db.pool())
        .await?;
        Ok(id)
    }
}

// ── Short ID helpers ─────────────────────────────────────────────────────────

/// Derive a 4-char base36 short ID from the last 4 bytes of a UUIDv7.
fn short_id_from_uuid(id: &uuid::Uuid) -> String {
    let bytes = id.as_bytes();
    let n = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    encode_base36(n % 1_679_616) // 36^4
}

/// Encode `n` (0..1_679_615) as a zero-padded 4-char base36 string.
fn encode_base36(mut n: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 4];
    for i in (0..4).rev() {
        buf[i] = CHARS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

// ── Dynamic query helpers ────────────────────────────────────────────────────

fn epic_build_where(
    project_id: &Option<String>,
    status: &Option<String>,
    text: &Option<String>,
    param_offset: usize,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(p) = project_id {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("project_id = {ph}"));
        params.push(SqlParam::Text(p.clone()));
    }

    if let Some(s) = status {
        let ph = format!("${}", param_offset + params.len() + 1);
        clauses.push(format!("status = {ph}"));
        params.push(SqlParam::Text(s.clone()));
    }
    if let Some(t) = text {
        let ph_a = format!("${}", param_offset + params.len() + 1);
        let ph_b = format!("${}", param_offset + params.len() + 2);
        clauses.push(format!("(title LIKE {ph_a} OR description LIKE {ph_b})"));
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

fn epic_sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        _ => "created_at ASC",
    }
}

async fn short_id_exists(pool: &sqlx::PgPool, table: &str, short_id: &str) -> Result<bool> {
    // NOTE: dynamic SQL (table name interpolated; values are internal constants only) — compile-time check not possible
    // Postgres `EXISTS(...)` returns BOOLEAN — decode as `bool`, not i64
    // (MySQL returned 0/1, so this was `<_, i64> > 0` pre-cutover and 500d
    // with "i64 (INT8) is not compatible with SQL type BOOL", breaking
    // epic AND task creation, which both mint short_ids via this helper).
    Ok(sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS(SELECT 1 FROM {table} WHERE short_id = $1)"
    ))
    .bind(short_id)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::Epic;

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

    #[derive(Debug, sqlx::FromRow)]
    struct TaskSnapshot {
        status: String,
        close_reason: Option<String>,
        closed_at: Option<String>,
    }

    #[derive(Debug, sqlx::FromRow)]
    struct ActivityRow {
        event_type: String,
        payload: serde_json::Value,
    }

    async fn insert_epic_child(db: &Database, epic: &Epic, short_id: &str, status: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let title = format!("Child {short_id}");
        let creator = crate::repositories::test_support::seed_test_user(db).await;
        sqlx::query(
            r#"INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                    issue_type, priority, owner, status, labels, acceptance_criteria, memory_refs, created_by_user_id)
               VALUES ($1, $2, $3, $4, $5, '', '', 'task', 1, '', $6,
                       '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $7)"#,
        )
        .bind(&id)
        .bind(&epic.project_id)
        .bind(short_id)
        .bind(&epic.id)
        .bind(&title)
        .bind(status)
        .bind(&creator)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn fetch_task_snapshot(db: &Database, task_id: &str) -> TaskSnapshot {
        sqlx::query_as::<_, TaskSnapshot>(
            "SELECT status, close_reason, closed_at FROM tasks WHERE id = $1",
        )
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
    }

    async fn fetch_activity_rows(db: &Database, task_id: &str) -> Vec<ActivityRow> {
        sqlx::query_as::<_, ActivityRow>(
            "SELECT event_type, payload FROM activity_log WHERE task_id = $1 ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(db.pool())
        .await
        .unwrap()
    }

    fn assert_activity_payload(
        rows: &[ActivityRow],
        event_type: &str,
        from_status: &str,
        to_status: &str,
        reason_key: Option<(&str, &str)>,
    ) {
        let row = rows
            .iter()
            .find(|row| row.event_type == event_type)
            .unwrap_or_else(|| panic!("missing activity event {event_type}; rows={rows:?}"));
        assert_eq!(row.payload["from_status"], from_status);
        assert_eq!(row.payload["to_status"], to_status);
        if event_type.starts_with("parent_child_") {
            assert_eq!(row.payload["entry_point"], "epic_close");
        }
        if let Some((key, expected)) = reason_key {
            assert_eq!(row.payload[key], expected);
        }
    }

    async fn insert_proposal_link(db: &Database, epic: &Epic, status: &str) -> String {
        let proposal_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("p{}", &proposal_id.replace('-', "")[..3]);
        sqlx::query(
            r#"INSERT INTO proposals
                    (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq)
               VALUES ($1, $2, 'Proposal', '', 'markdown', '[]'::jsonb, $3, 1)"#,
        )
        .bind(&proposal_id)
        .bind(&short_id)
        .bind(status)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO proposal_epics (proposal_id, epic_id, project_id) VALUES ($1, $2, $3)",
        )
        .bind(&proposal_id)
        .bind(&epic.id)
        .bind(&epic.project_id)
        .execute(db.pool())
        .await
        .unwrap();
        proposal_id
    }

    async fn add_task_blocker(db: &Database, task_id: &str, blocking_task_id: &str) {
        sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES ($1, $2)")
            .bind(task_id)
            .bind(blocking_task_id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_epic() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo
            .create("My Epic", "", "🚀", "#8b5cf6", "user@example.com", None)
            .await
            .unwrap();
        assert_eq!(epic.title, "My Epic");
        assert_eq!(epic.status, "open");
        assert_eq!(epic.short_id.len(), 4);

        let fetched = repo.get(&epic.id).await.unwrap().unwrap();
        assert_eq!(fetched.title, "My Epic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn short_id_lookup() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Lookup", "", "", "", "", None).await.unwrap();
        let found = repo.get_by_short_id(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(found.id, epic.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        repo.create("Event Epic", "", "", "", "", None)
            .await
            .unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "created");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.title, "Event Epic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo.create("Old", "", "", "", "", None).await.unwrap();
        captured.lock().unwrap().clear();

        let updated = repo
            .update(
                &epic.id,
                EpicUpdateInput {
                    title: "New",
                    description: "desc",
                    emoji: "🎯",
                    color: "#fff",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.title, "New");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.title, "New");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_disposes_ready_and_intervention_children_with_activity() {
        let (bus, captured) = capturing_bus();
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), bus);
        let epic = repo
            .create("Disposition close buckets", "", "", "", "", None)
            .await
            .unwrap();

        let cases = [
            ("dc01", "open"),
            ("dc02", "needs_lead_intervention"),
            ("dc03", "in_lead_intervention"),
        ];
        let mut task_ids = Vec::new();
        for (short_id, status) in cases {
            task_ids.push((
                insert_epic_child(&db, &epic, short_id, status).await,
                status,
            ));
        }
        captured.lock().unwrap().clear();

        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        for (task_id, from_status) in &task_ids {
            let task = fetch_task_snapshot(&db, task_id).await;
            assert_eq!(task.status, "closed");
            assert_eq!(task.close_reason.as_deref(), Some("parent_closed"));
            assert!(task.closed_at.is_some());

            let activity = fetch_activity_rows(&db, task_id).await;
            assert_activity_payload(
                &activity,
                "status_changed",
                from_status,
                "closed",
                Some(("reason", "parent_closed")),
            );
            assert_activity_payload(
                &activity,
                "parent_child_disposed",
                from_status,
                "closed",
                None,
            );
        }

        let events = captured.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.entity_type == "task" && event.action == "updated")
                .count(),
            task_ids.len()
        );
        assert!(
            events
                .iter()
                .any(|event| event.entity_type == "epic" && event.action == "updated")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_parks_in_flight_review_and_pr_children_with_activity() {
        let (bus, captured) = capturing_bus();
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), bus);
        let epic = repo
            .create("Disposition park buckets", "", "", "", "", None)
            .await
            .unwrap();

        let cases = [
            ("dp01", "in_progress", "parent_closed_in_flight"),
            ("dp02", "needs_task_review", "parent_closed_in_flight"),
            ("dp03", "in_task_review", "parent_closed_in_flight"),
            ("dp04", "approved", "parent_closed_pr_active"),
            ("dp05", "pr_draft", "parent_closed_pr_active"),
            ("dp06", "pr_review", "parent_closed_pr_active"),
        ];
        let mut task_ids = Vec::new();
        for (short_id, status, reason) in cases {
            task_ids.push((
                insert_epic_child(&db, &epic, short_id, status).await,
                status,
                reason,
            ));
        }
        captured.lock().unwrap().clear();

        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        for (task_id, from_status, park_reason) in &task_ids {
            let task = fetch_task_snapshot(&db, task_id).await;
            assert_eq!(task.status, "needs_lead_intervention");
            assert!(task.close_reason.is_none());
            assert!(task.closed_at.is_none());

            let activity = fetch_activity_rows(&db, task_id).await;
            assert_activity_payload(
                &activity,
                "status_changed",
                from_status,
                "needs_lead_intervention",
                Some(("reason", park_reason)),
            );
            assert_activity_payload(
                &activity,
                "parent_child_parked",
                from_status,
                "needs_lead_intervention",
                Some(("park_reason", park_reason)),
            );
        }

        let events = captured.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.entity_type == "task" && event.action == "updated")
                .count(),
            task_ids.len()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_leaves_already_closed_child_unchanged() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let epic = repo
            .create("Already terminal child", "", "", "", "", None)
            .await
            .unwrap();
        let task_id = insert_epic_child(&db, &epic, "dt01", "closed").await;
        sqlx::query(
            r#"UPDATE tasks
                  SET close_reason = 'completed',
                      closed_at = '2026-01-01T00:00:00.000Z'
                WHERE id = $1"#,
        )
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();

        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");

        let task = fetch_task_snapshot(&db, &task_id).await;
        assert_eq!(task.status, "closed");
        assert_eq!(task.close_reason.as_deref(), Some("completed"));
        assert_eq!(task.closed_at.as_deref(), Some("2026-01-01T00:00:00.000Z"));
        assert!(fetch_activity_rows(&db, &task_id).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_commits_parent_when_other_open_proposal_parent_retains_child() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let epic = repo
            .create("Other proposal retained child", "", "", "", "", None)
            .await
            .unwrap();
        let proposal_id = insert_proposal_link(&db, &epic, "building").await;
        let task_id = insert_epic_child(&db, &epic, "dr01", "open").await;

        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        let task = fetch_task_snapshot(&db, &task_id).await;
        assert_eq!(task.status, "open");
        assert!(task.close_reason.is_none());
        assert!(task.closed_at.is_none());
        assert!(fetch_activity_rows(&db, &task_id).await.is_empty());

        let evidence: (String,) = sqlx::query_as(
            r#"SELECT p.id
                 FROM proposal_epics pe
                 JOIN proposals p ON p.id = pe.proposal_id
                WHERE pe.epic_id = $1 AND p.status = 'building'"#,
        )
        .bind(&epic.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(evidence.0, proposal_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_retains_external_dependent_but_allows_internal_blocker_cascade() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let closing_epic = repo
            .create("Dependency guard closing", "", "", "", "", None)
            .await
            .unwrap();
        let other_epic = repo
            .create("Dependency guard external", "", "", "", "", None)
            .await
            .unwrap();

        let retained_blocker = insert_epic_child(&db, &closing_epic, "dg01", "open").await;
        let external_dependent = insert_epic_child(&db, &other_epic, "dg02", "open").await;
        add_task_blocker(&db, &external_dependent, &retained_blocker).await;

        let internal_blocker = insert_epic_child(&db, &closing_epic, "dg03", "open").await;
        let internal_dependent = insert_epic_child(&db, &closing_epic, "dg04", "in_progress").await;
        add_task_blocker(&db, &internal_dependent, &internal_blocker).await;

        let closed = repo.close(&closing_epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        let retained = fetch_task_snapshot(&db, &retained_blocker).await;
        assert_eq!(retained.status, "open");
        assert!(retained.close_reason.is_none());
        assert!(fetch_activity_rows(&db, &retained_blocker).await.is_empty());

        let dependent = fetch_task_snapshot(&db, &external_dependent).await;
        assert_eq!(dependent.status, "open");

        let blocker = fetch_task_snapshot(&db, &internal_blocker).await;
        assert_eq!(blocker.status, "closed");
        assert_eq!(blocker.close_reason.as_deref(), Some("parent_closed"));
        assert_activity_payload(
            &fetch_activity_rows(&db, &internal_blocker).await,
            "parent_child_disposed",
            "open",
            "closed",
            None,
        );

        let internal = fetch_task_snapshot(&db, &internal_dependent).await;
        assert_eq!(internal.status, "needs_lead_intervention");
        assert!(internal.close_reason.is_none());
        assert_activity_payload(
            &fetch_activity_rows(&db, &internal_dependent).await,
            "parent_child_parked",
            "in_progress",
            "needs_lead_intervention",
            Some(("park_reason", "parent_closed_in_flight")),
        );

        let evidence: (String,) = sqlx::query_as(
            r#"SELECT t.id
                 FROM blockers b
                 JOIN tasks t ON t.id = b.task_id
                WHERE b.blocking_task_id = $1
                  AND t.epic_id <> $2
                  AND t.status <> 'closed'"#,
        )
        .bind(&retained_blocker)
        .bind(&closing_epic.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(evidence.0, external_dependent);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo
            .create("Closeable", "", "", "", "", None)
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.id, epic.id);
        assert_eq!(e.status, "closed");
        assert!(e.closed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo.create("Reopen", "", "", "", "", None).await.unwrap();
        repo.close(&epic.id).await.unwrap();
        captured.lock().unwrap().clear();

        let reopened = repo.reopen(&epic.id).await.unwrap();
        assert_eq!(reopened.status, "open");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        let e: Epic = serde_json::from_value(events[0].payload.clone()).unwrap();
        assert_eq!(e.id, epic.id);
        assert_eq!(e.status, "open");
        assert!(e.closed_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_events_carry_proposal_swimlane_labels() {
        let (bus, captured) = capturing_bus();
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), bus);

        let epic = repo.create("Linked", "", "", "", "", None).await.unwrap();
        let proposal_id = insert_proposal_link(&db, &epic, "building").await;
        sqlx::query("UPDATE epics SET proposal_id = $1 WHERE id = $2")
            .bind(&proposal_id)
            .bind(&epic.id)
            .execute(db.pool())
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        repo.update_memory_refs(&epic.id, r#"["design/roadmap"]"#)
            .await
            .unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "updated");
        // Live SSE payloads must match the epic_list enrichment: without
        // these labels the board drops freshly created/linked epics into the
        // "No proposal" swimlane until a full page reload.
        let payload = &events[0].payload;
        assert_eq!(payload["proposal_id"].as_str().unwrap(), proposal_id);
        assert_eq!(payload["proposal_status"], "building");
        assert_eq!(payload["proposal_title"], "Proposal");
        assert!(payload["proposal_short_id"].is_string());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn epic_event_without_proposal_omits_labels() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        repo.create("Unlinked", "", "", "", "", None).await.unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "created");
        assert!(events[0].payload.get("proposal_status").is_none());
        assert!(events[0].payload.get("proposal_short_id").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_emits_event() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let epic = repo
            .create("Delete me", "", "", "", "", None)
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        repo.delete(&epic.id).await.unwrap();
        assert!(repo.get(&epic.id).await.unwrap().is_none());

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "epic");
        assert_eq!(events[0].action, "deleted");
        assert_eq!(events[0].payload["id"].as_str().unwrap(), epic.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_by_id_and_short_id() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Resolve", "", "", "", "", None).await.unwrap();

        let by_id = repo.resolve(&epic.id).await.unwrap().unwrap();
        assert_eq!(by_id.id, epic.id);

        let by_short = repo.resolve(&epic.short_id).await.unwrap().unwrap();
        assert_eq!(by_short.id, epic.id);

        assert!(repo.resolve("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_closed() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Reopen", "", "", "", "", None).await.unwrap();
        repo.close(&epic.id).await.unwrap();

        let reopened = repo.reopen(&epic.id).await.unwrap();
        assert_eq!(reopened.status, "open");
        assert!(reopened.closed_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reopen_from_open_is_error() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());

        let epic = repo.create("Open", "", "", "", "", None).await.unwrap();
        assert!(repo.reopen(&epic.id).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_counts_aggregation() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());

        let epic = repo.create("Counts", "", "", "", "", None).await.unwrap();
        let pool = db.pool();
        let creator = crate::repositories::test_support::seed_test_user(&db).await;

        // Insert tasks directly via SQL.
        for short in ["t001", "t002"] {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query!(
                "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                    issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
                 VALUES ($1, $2, $3, $4, 'T', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
                id,
                epic.project_id,
                short,
                epic.id,
                creator
            )
            .execute(pool)
            .await
            .unwrap();
        }
        let t3_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
             VALUES ($1, $2, 't003', $3, 'T3', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $4)",
            t3_id,
            epic.project_id,
            epic.id,
            creator
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query!("UPDATE tasks SET status = 'closed' WHERE id = $1", t3_id)
            .execute(pool)
            .await
            .unwrap();

        let counts = repo.task_counts(&epic.id).await.unwrap();
        assert_eq!(counts.task_count, 3);
        assert_eq!(counts.open_count, 2);
        assert_eq!(counts.closed_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_with_count_returns_child_count() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());

        let epic = repo.create("Delete", "", "", "", "", None).await.unwrap();
        let pool = db.pool();
        let creator = crate::repositories::test_support::seed_test_user(&db).await;

        for short in ["t001", "t002"] {
            let id = uuid::Uuid::now_v7().to_string();
            sqlx::query!(
                "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                    issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
                 VALUES ($1, $2, $3, $4, 'T', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
                id,
                epic.project_id,
                short,
                epic.id,
                creator
            )
            .execute(pool)
            .await
            .unwrap();
        }

        let count = repo.delete_with_count(&epic.id).await.unwrap();
        assert_eq!(count, 2);
        assert!(repo.get(&epic.id).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_defaults_to_open() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let epic = repo.create("New Epic", "", "", "", "", None).await.unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_with_explicit_open_status() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let project_id = repo.ensure_default_project_id().await.unwrap();
        let epic = repo
            .create_for_project(
                &project_id,
                EpicCreateInput {
                    title: "Open Epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: Some("open"),
                    auto_breakdown: None,
                    originating_adr_id: None,
                    blocked_by: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_from_open() {
        let repo = EpicRepository::new(test_db(), EventBus::noop());
        let epic = repo.create("E", "", "", "", "", None).await.unwrap();
        assert_eq!(epic.status, "open");
        let closed = repo.close(&epic.id).await.unwrap();
        assert_eq!(closed.status, "closed");
        assert!(closed.closed_at.is_some());
    }

    async fn insert_project(db: &Database, owner: &str) -> String {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_sources_add_list_remove() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let epic = repo
            .create("Migration", "", "", "", "", None)
            .await
            .unwrap();
        let src_id = insert_project(&db, "legacy").await;

        assert!(repo.read_sources(&epic.id).await.unwrap().is_empty());

        repo.add_read_source(&epic.id, &src_id).await.unwrap();
        // Idempotent — a second add does not duplicate the row.
        repo.add_read_source(&epic.id, &src_id).await.unwrap();
        assert_eq!(
            repo.read_sources(&epic.id).await.unwrap(),
            vec![src_id.clone()]
        );
        // read_sources_for_task resolves through the epic.
        assert_eq!(
            repo.read_sources_for_task(Some(&epic.id)).await.unwrap(),
            vec![src_id.clone()]
        );
        assert!(repo.read_sources_for_task(None).await.unwrap().is_empty());

        repo.remove_read_source(&epic.id, &src_id).await.unwrap();
        assert!(repo.read_sources(&epic.id).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_sources_cascade_on_epic_delete() {
        let db = test_db();
        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let epic = repo.create("E", "", "", "", "", None).await.unwrap();
        let src_id = insert_project(&db, "src").await;
        repo.add_read_source(&epic.id, &src_id).await.unwrap();

        repo.delete(&epic.id).await.unwrap();

        let remaining: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "c!: i64" FROM epic_read_sources WHERE epic_id = $1"#,
            epic.id
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(remaining, 0);
    }

    // ── Regression tests: race-condition + propagation (i528-1 §4) ──────────

    /// Regression test: `emit_unblocked_epics` correctly emits `epic.updated`
    /// for dependents that become fully unblocked when their last blocker closes.
    /// Also verifies that dependents with remaining open blockers are NOT emitted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_unblocked_epics_fires_for_fully_unblocked_dependents() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        // A: foundation epic.
        let a = repo
            .create("Foundation A", "", "", "", "", None)
            .await
            .unwrap();
        // B: another blocker.
        let b = repo
            .create("Blocker B", "", "", "", "", None)
            .await
            .unwrap();
        // C: depends on both A and B.
        let c = repo
            .create("Dependent C", "", "", "", "", None)
            .await
            .unwrap();

        // Wire C.blocked_by = A and C.blocked_by = B.
        repo.add_blocker(&c.id, &a.id).await.unwrap();
        repo.add_blocker(&c.id, &b.id).await.unwrap();
        captured.lock().unwrap().clear();

        // Close A — B is still open, so C is NOT fully unblocked.
        repo.close(&a.id).await.unwrap();

        // emit_unblocked_epics is called internally by close().
        // Since B is still open, C should NOT have been emitted.
        let events_after_a = captured.lock().unwrap().clone();
        let c_emitted: Vec<_> = events_after_a
            .iter()
            .filter(|ev| ev.action == "updated" && ev.entity_type == "epic")
            .filter_map(|ev| {
                let e: Epic = serde_json::from_value(ev.payload.clone()).ok()?;
                if e.id == c.id { Some(e) } else { None }
            })
            .collect();
        assert!(
            c_emitted.is_empty(),
            "C must NOT be emitted while B is still open"
        );

        // Now close B — C should be fully unblocked.
        captured.lock().unwrap().clear();
        repo.close(&b.id).await.unwrap();

        let events_after_b = captured.lock().unwrap().clone();
        let c_unblocked: Vec<_> = events_after_b
            .iter()
            .filter(|ev| ev.action == "updated" && ev.entity_type == "epic")
            .filter_map(|ev| {
                let e: Epic = serde_json::from_value(ev.payload.clone()).ok()?;
                if e.id == c.id { Some(e) } else { None }
            })
            .collect();
        assert_eq!(
            c_unblocked.len(),
            1,
            "C must be emitted once when its last blocker (B) closes"
        );
        assert_eq!(c_unblocked[0].id, c.id);
    }

    /// Regression test: `emit_unblocked_epics` only fires for OPEN dependents.
    /// A closed dependent must NOT be re-emitted even if its blocker closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn emit_unblocked_epics_skips_closed_dependents() {
        let (bus, captured) = capturing_bus();
        let repo = EpicRepository::new(test_db(), bus);

        let blocker = repo.create("Blocker", "", "", "", "", None).await.unwrap();
        let dependent = repo
            .create("Dependent", "", "", "", "", None)
            .await
            .unwrap();

        repo.add_blocker(&dependent.id, &blocker.id).await.unwrap();

        // Close the dependent first (it was closed before its blocker).
        repo.close(&dependent.id).await.unwrap();
        captured.lock().unwrap().clear();

        // Now close the blocker.
        repo.close(&blocker.id).await.unwrap();

        // The closed dependent should NOT have been emitted.
        let events = captured.lock().unwrap().clone();
        let dependent_emitted: Vec<_> = events
            .iter()
            .filter(|ev| ev.action == "updated" && ev.entity_type == "epic")
            .filter_map(|ev| {
                let e: Epic = serde_json::from_value(ev.payload.clone()).ok()?;
                if e.id == dependent.id { Some(e) } else { None }
            })
            .collect();
        assert!(
            dependent_emitted.is_empty(),
            "closed dependent must NOT be re-emitted when its blocker closes"
        );
    }

    #[test]
    fn encode_base36_roundtrip() {
        assert_eq!(encode_base36(0), "0000");
        assert_eq!(encode_base36(1_679_615), "zzzz");
        for s in [encode_base36(12345), encode_base36(999999)] {
            assert_eq!(s.len(), 4);
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() && !c.is_uppercase())
            );
        }
    }
}
