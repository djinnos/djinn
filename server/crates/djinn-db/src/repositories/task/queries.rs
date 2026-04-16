use super::task_select_where_id;
use super::*;
use crate::SessionRepository;
use djinn_core::models::IssueType;

const REPEATED_REOPEN_MISMATCH_THRESHOLD: i64 = 3;

fn toolset_for_role(role: &str) -> &'static [&'static str] {
    match role {
        "planner" => &[
            "task_create",
            "epic_update",
            "memory_ref_update",
            "reprioritization",
        ],
        "lead" => &[
            "lead_intervention",
            "force_close",
            "replacement_tasks",
            "decomposition",
        ],
        "reviewer" => &["task_review"],
        "architect" => &["board_health_review", "stuck_task_review"],
        _ => &["code_changes", "tests", "verification_fix"],
    }
}

fn infer_expected_role_for_task(task: &Task) -> Option<(&'static str, Vec<String>)> {
    let mut planner_signals = Vec::new();

    if matches!(task.issue_type.as_str(), "planning" | "decomposition") {
        planner_signals.push("issue_type:planning".to_string());
        return Some(("planner", planner_signals));
    }

    let haystack = format!(
        "{}\n{}\n{}\n{}",
        task.title.to_lowercase(),
        task.description.to_lowercase(),
        task.design.to_lowercase(),
        task.acceptance_criteria.to_lowercase()
    );

    let add_signal_once = |signals: &mut Vec<String>, signal: &str| {
        if !signals.iter().any(|existing| existing == signal) {
            signals.push(signal.to_string());
        }
    };

    for (needle, signal) in [
        ("task_create", "requires:task_create"),
        ("epic_update", "requires:epic_update"),
        ("memory_ref", "requires:memory_ref_update"),
        ("memory refs", "requires:memory_ref_update"),
        ("re-priorit", "requires:reprioritization"),
        ("repriorit", "requires:reprioritization"),
        ("decompos", "requires:decomposition"),
        ("plan next wave", "requires:planning"),
        ("planning task", "requires:planning"),
    ] {
        if haystack.contains(needle) {
            add_signal_once(&mut planner_signals, signal);
        }
    }

    let mut lead_signals = Vec::new();
    for (needle, signal) in [
        ("lead intervention", "requires:lead_intervention"),
        ("force_close", "requires:force_close"),
        ("replacement task", "requires:replacement_tasks"),
        ("decompose into subtasks", "requires:decomposition"),
    ] {
        if haystack.contains(needle) {
            add_signal_once(&mut lead_signals, signal);
        }
    }

    if !planner_signals.is_empty() {
        return Some(("planner", planner_signals));
    }
    if !lead_signals.is_empty() {
        return Some(("lead", lead_signals));
    }

    if IssueType::parse(&task.issue_type)
        .map(|issue_type| issue_type.uses_simple_lifecycle())
        .unwrap_or(false)
    {
        return None;
    }

    None
}

fn dispatched_role_for_task(task: &Task) -> &'static str {
    match task.status.as_str() {
        "needs_task_review" | "in_task_review" => "reviewer",
        "needs_lead_intervention" | "in_lead_intervention" => "lead",
        _ => match task.issue_type.as_str() {
            "planning" | "decomposition" => "planner",
            "spike" | "review" => "architect",
            _ => "worker",
        },
    }
}

fn role_tool_mismatch_reason(
    total_reopen_count: i64,
    expected_role: &str,
    dispatched_role: &str,
) -> String {
    let expected_tools = toolset_for_role(expected_role).join(", ");
    let dispatched_tools = toolset_for_role(dispatched_role).join(", ");
    format!(
        "Repeated reopen churn ({total_reopen_count} reopens) suggests this task needs the {expected_role} toolset ({expected_tools}) rather than the currently routed {dispatched_role} toolset ({dispatched_tools})."
    )
}

async fn list_board_health_mismatch_candidates(repo: &TaskRepository) -> Result<Vec<Task>> {
    let list = repo
        .list_filtered(ListQuery {
            project_id: None,
            status: None,
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            sort: "updated_desc".to_string(),
            limit: 10_000,
            offset: 0,
        })
        .await?;
    Ok(list.tasks)
}

impl TaskRepository {
    /// List tasks ready to start: status='open' with no unresolved blockers.
    pub async fn list_ready(&self, query: ReadyQuery) -> Result<Vec<Task>> {
        self.db.ensure_initialized().await?;
        let mut clauses: Vec<String> = vec![
            "t.`status` = 'open'".to_owned(),
            "NOT EXISTS (
                 SELECT 1 FROM blockers b2
                 JOIN tasks bt ON bt.id = b2.blocking_task_id
                 WHERE b2.task_id = t.id
                    AND bt.`status` != 'closed'
             )"
            .to_owned(),
        ];
        let mut params: Vec<SqlParam> = Vec::new();

        if let Some(project_id) = &query.project_id {
            clauses.push("t.project_id = ?".to_owned());
            params.push(SqlParam::Text(project_id.clone()));
        }

        if let Some(it) = &query.issue_type {
            if let Some(neg) = it.strip_prefix('!') {
                clauses.push("t.issue_type != ?".to_owned());
                params.push(SqlParam::Text(neg.to_owned()));
            } else {
                clauses.push("t.issue_type = ?".to_owned());
                params.push(SqlParam::Text(it.clone()));
            }
        }
        if let Some(lbl) = &query.label {
            clauses.push("JSON_CONTAINS(t.labels, JSON_QUOTE(?), '$')".to_owned());
            params.push(SqlParam::Text(lbl.clone()));
        }
        if let Some(owner) = &query.owner {
            clauses.push("t.owner = ?".to_owned());
            params.push(SqlParam::Text(owner.clone()));
        }
        if let Some(pmax) = query.priority_max {
            clauses.push("t.priority <= ?".to_owned());
            params.push(SqlParam::Integer(pmax));
        }

        let where_sql = clauses.join(" AND ");
        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let sql = format!(
            "SELECT t.id, t.project_id, t.short_id, t.epic_id, t.title, t.description, t.design,
                    t.issue_type, t.`status`, t.priority, t.owner, t.labels,
                    t.acceptance_criteria, t.reopen_count, t.continuation_count,
                    t.verification_failure_count,
                    t.total_reopen_count, t.total_verification_failure_count,
                    t.intervention_count, t.last_intervention_at,
                    t.created_at, t.updated_at, t.closed_at,
                    t.close_reason, t.merge_commit_sha, t.pr_url, t.merge_conflict_metadata, t.memory_refs
             FROM tasks t
             WHERE {where_sql}
             ORDER BY t.priority ASC, t.created_at ASC
             LIMIT ?"
        );
        let mut q = sqlx::query_as::<_, Task>(&sql);
        for p in params {
            q = match p {
                SqlParam::Text(s) => q.bind(s),
                SqlParam::Integer(i) => q.bind(i),
            };
        }
        Ok(q.bind(query.limit).fetch_all(self.db.pool()).await?)
    }

    /// Atomically claim the highest-priority, oldest ready task and transition it
    /// to `in_progress`.
    ///
    /// "Ready" means `status = 'open'` with no unresolved blockers.  All filtering
    /// and the Start transition happen inside a single write transaction, so two
    /// concurrent callers can never claim the same task.
    ///
    /// Returns `None` when no task matches the query.
    pub async fn claim(
        &self,
        query: ReadyQuery,
        actor_id: &str,
        actor_role: &str,
    ) -> Result<Option<Task>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        // Build the same WHERE as list_ready, then LIMIT 1 for the claim.
        let mut clauses: Vec<String> = vec![
            "t.`status` = 'open'".to_owned(),
            "NOT EXISTS (
                 SELECT 1 FROM blockers b2
                 JOIN tasks bt ON bt.id = b2.blocking_task_id
                 WHERE b2.task_id = t.id
                    AND bt.`status` != 'closed'
             )"
            .to_owned(),
        ];
        let mut params: Vec<SqlParam> = Vec::new();

        if let Some(project_id) = &query.project_id {
            clauses.push("t.project_id = ?".to_owned());
            params.push(SqlParam::Text(project_id.clone()));
        }

        if let Some(it) = &query.issue_type {
            if let Some(neg) = it.strip_prefix('!') {
                clauses.push("t.issue_type != ?".to_owned());
                params.push(SqlParam::Text(neg.to_owned()));
            } else {
                clauses.push("t.issue_type = ?".to_owned());
                params.push(SqlParam::Text(it.clone()));
            }
        }
        if let Some(lbl) = &query.label {
            clauses.push("JSON_CONTAINS(t.labels, JSON_QUOTE(?), '$')".to_owned());
            params.push(SqlParam::Text(lbl.clone()));
        }
        if let Some(owner) = &query.owner {
            clauses.push("t.owner = ?".to_owned());
            params.push(SqlParam::Text(owner.clone()));
        }
        if let Some(pmax) = query.priority_max {
            clauses.push("t.priority <= ?".to_owned());
            params.push(SqlParam::Integer(pmax));
        }

        let where_sql = clauses.join(" AND ");
        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let sql = format!(
            "SELECT t.id, t.project_id, t.short_id, t.epic_id, t.title, t.description, t.design,
                    t.issue_type, t.`status`, t.priority, t.owner, t.labels,
                    t.acceptance_criteria, t.reopen_count, t.continuation_count,
                    t.verification_failure_count,
                    t.total_reopen_count, t.total_verification_failure_count,
                    t.intervention_count, t.last_intervention_at,
                    t.created_at, t.updated_at, t.closed_at,
                    t.close_reason, t.merge_commit_sha, t.pr_url, t.merge_conflict_metadata, t.memory_refs
             FROM tasks t
             WHERE {where_sql}
             ORDER BY t.priority ASC, t.created_at ASC
             LIMIT 1"
        );
        let mut candidate_q = sqlx::query_as::<_, Task>(&sql);
        for p in params {
            candidate_q = match p {
                SqlParam::Text(s) => candidate_q.bind(s),
                SqlParam::Integer(i) => candidate_q.bind(i),
            };
        }

        let candidate: Option<Task> = candidate_q.fetch_optional(&mut *tx).await?;

        let task = match candidate {
            None => {
                tx.commit().await?;
                return Ok(None);
            }
            Some(t) => t,
        };

        // Apply Start transition: open → in_progress.
        sqlx::query!(
            "UPDATE tasks SET `status` = 'in_progress',
                 updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            task.id
        )
        .execute(&mut *tx)
        .await?;

        let activity_id = uuid::Uuid::now_v7().to_string();
        let payload = serde_json::json!({
            "from_status": "open",
            "to_status":   "in_progress",
        })
        .to_string();
        sqlx::query!(
            "INSERT INTO activity_log
                (id, task_id, actor_id, actor_role, event_type, payload)
             VALUES (?, ?, ?, ?, 'status_changed', ?)",
            activity_id,
            task.id,
            actor_id,
            actor_role,
            payload
        )
        .execute(&mut *tx)
        .await?;

        let task = task_select_where_id!(&task.id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        let task = Some(task);

        if let Some(ref t) = task {
            self.events.send(DjinnEventEnvelope::task_updated(t, false));
        }
        Ok(task)
    }

    /// List tasks with filters, sorting, and pagination.
    pub async fn list_filtered(&self, query: ListQuery) -> Result<ListResult> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = build_where(
            &query.project_id,
            &query.status,
            &query.issue_type,
            query.priority,
            &query.label,
            &query.text,
            &query.parent,
        );
        let order_sql = sort_to_sql(&query.sort);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM tasks WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            total_q = match p {
                SqlParam::Text(s) => total_q.bind(s.clone()),
                SqlParam::Integer(i) => total_q.bind(*i),
            };
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        // NOTE: dynamic SQL (WHERE + ORDER clauses built from request) — compile-time check not possible
        let sql = format!(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    status, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs,
                    (SELECT COUNT(*) FROM blockers b
                     JOIN tasks bt ON b.blocking_task_id = bt.id
                     WHERE b.task_id = tasks.id AND bt.`status` != 'closed') AS unresolved_blocker_count
             FROM tasks WHERE {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
        );
        let mut task_q = sqlx::query_as::<_, Task>(&sql);
        for p in &params {
            task_q = match p {
                SqlParam::Text(s) => task_q.bind(s.clone()),
                SqlParam::Integer(i) => task_q.bind(*i),
            };
        }
        let tasks = task_q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

        Ok(ListResult {
            tasks,
            total_count: total,
        })
    }

    /// Count tasks with optional grouping.
    pub async fn count_grouped(&self, query: CountQuery) -> Result<serde_json::Value> {
        self.db.ensure_initialized().await?;
        let (where_sql, params) = build_where(
            &query.project_id,
            &query.status,
            &query.issue_type,
            query.priority,
            &query.label,
            &query.text,
            &query.parent,
        );

        match query.group_by.as_deref() {
            Some(gb) => {
                let col = match gb {
                    "status" => "`status`",
                    "priority" => "priority",
                    "issue_type" => "issue_type",
                    "epic" => "epic_id",
                    other => {
                        return Err(Error::Internal(format!("unknown group_by: {other}")));
                    }
                };
                // NOTE: dynamic SQL (group_by column chosen at runtime) — compile-time check not possible
                let sql = format!(
                    "SELECT COALESCE(CAST({col} AS CHAR), ''), COUNT(*)
                     FROM tasks WHERE {where_sql}
                     GROUP BY {col} ORDER BY COUNT(*) DESC"
                );
                let mut groups_q = sqlx::query_as::<_, (String, i64)>(&sql);
                for p in &params {
                    groups_q = match p {
                        SqlParam::Text(s) => groups_q.bind(s.clone()),
                        SqlParam::Integer(i) => groups_q.bind(*i),
                    };
                }
                let groups = groups_q
                    .fetch_all(self.db.pool())
                    .await?
                    .into_iter()
                    .map(|(key, count)| serde_json::json!({"key": key, "count": count}))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({ "groups": groups }))
            }
            None => {
                // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
                let sql = format!("SELECT COUNT(*) FROM tasks WHERE {where_sql}");
                let mut total_q = sqlx::query_scalar::<_, i64>(&sql);
                for p in &params {
                    total_q = match p {
                        SqlParam::Text(s) => total_q.bind(s.clone()),
                        SqlParam::Integer(i) => total_q.bind(*i),
                    };
                }
                let total = total_q.fetch_one(self.db.pool()).await?;
                Ok(serde_json::json!({ "total_count": total }))
            }
        }
    }

    /// Aggregate board health report: epic stats, stale in_progress tasks, review queue.
    pub async fn board_health(&self, stale_hours: i64) -> Result<serde_json::Value> {
        self.db.ensure_initialized().await?;
        // Per-epic task counts and % complete.
        // NOTE: dynamic row extraction via sqlx::Row::get — macro conversion would require a full refactor to typed rows
        let epic_rows = sqlx::query(
            "SELECT e.id, e.short_id, e.title,
                    COUNT(t.id) AS total,
                    SUM(CASE WHEN t.`status` = 'closed' THEN 1 ELSE 0 END) AS closed,
                    SUM(CASE WHEN t.`status` IN (
                        'needs_task_review','in_task_review','closed'
                    ) THEN 1 ELSE 0 END) AS in_review,
                    MIN(CASE WHEN t.`status` IN (
                        'needs_task_review','in_task_review','closed'
                    ) THEN t.updated_at ELSE NULL END) AS oldest_review_at,
                    SUM(CASE WHEN t.`status` IN ('approved','pr_draft','pr_review') THEN 1 ELSE 0 END) AS pr_ready
             FROM epics e
             LEFT JOIN tasks t ON t.epic_id = e.id
             GROUP BY e.id
             ORDER BY e.title",
        )
        .fetch_all(self.db.pool())
        .await?;
        let epic_stats: Vec<serde_json::Value> = epic_rows
            .into_iter()
            .map(|row| {
                use sqlx::Row;
                let total: i64 = row.get(3);
                let closed: i64 = row.get::<Option<i64>, _>(4).unwrap_or(0);
                let in_review: i64 = row.get::<Option<i64>, _>(5).unwrap_or(0);
                let oldest_review_at: Option<String> = row.get(6);
                let pr_ready: i64 = row.get::<Option<i64>, _>(7).unwrap_or(0);
                let pct = if total > 0 {
                    (closed as f64 / total as f64 * 1000.0).round() / 10.0
                } else {
                    0.0
                };
                serde_json::json!({
                    "epic_id":          row.get::<String, _>(0),
                    "short_id":         row.get::<String, _>(1),
                    "title":            row.get::<String, _>(2),
                    "total":            total,
                    "closed":           closed,
                    "in_review":        in_review,
                    "pr_ready":         pr_ready,
                    "pct_complete":     pct,
                    "oldest_review_at": oldest_review_at,
                })
            })
            .collect();

        // Stale tasks: in_progress longer than the threshold.
        // NOTE: dynamic SQL (stale_hours inlined) — compile-time check not possible
        let stale_sql = format!(
            "SELECT t.id, t.short_id, t.title, t.`status`, t.updated_at, t.owner,
                    e.short_id AS epic_short_id
             FROM tasks t
             JOIN epics e ON t.epic_id = e.id
             WHERE t.`status` = 'in_progress'
               AND t.updated_at < DATE_SUB(NOW(3), INTERVAL {stale_hours} HOUR)
             ORDER BY t.updated_at ASC"
        );
        let stale_rows = sqlx::query(&stale_sql).fetch_all(self.db.pool()).await?;
        let stale_tasks: Vec<serde_json::Value> = stale_rows
            .into_iter()
            .map(|row| {
                use sqlx::Row;
                serde_json::json!({
                    "id":            row.get::<String, _>(0),
                    "short_id":      row.get::<String, _>(1),
                    "title":         row.get::<String, _>(2),
                    "status":        row.get::<String, _>(3),
                    "updated_at":    row.get::<String, _>(4),
                    "owner":         row.get::<String, _>(5),
                    "epic_short_id": row.get::<String, _>(6),
                })
            })
            .collect();

        // Review queue: tasks waiting in any review status.
        // NOTE: dynamic row extraction via sqlx::Row::get — macro conversion would require a full refactor to typed rows
        let review_rows = sqlx::query(
            "SELECT t.id, t.short_id, t.title, t.`status`, t.updated_at,
                    e.short_id AS epic_short_id
             FROM tasks t
             JOIN epics e ON t.epic_id = e.id
             WHERE t.`status` IN ('needs_task_review','in_task_review','approved','pr_draft','pr_review','closed')
             ORDER BY t.updated_at ASC",
        )
        .fetch_all(self.db.pool())
        .await?;
        let review_queue: Vec<serde_json::Value> = review_rows
            .into_iter()
            .map(|row| {
                use sqlx::Row;
                serde_json::json!({
                    "id":            row.get::<String, _>(0),
                    "short_id":      row.get::<String, _>(1),
                    "title":         row.get::<String, _>(2),
                    "status":        row.get::<String, _>(3),
                    "updated_at":    row.get::<String, _>(4),
                    "epic_short_id": row.get::<String, _>(5),
                })
            })
            .collect();

        let session_repo = SessionRepository::new(self.db.clone(), self.events.clone());
        let mismatch_candidates = list_board_health_mismatch_candidates(self).await?;
        let mut repeated_reopen_role_tool_mismatches = Vec::new();

        for task in mismatch_candidates {
            if task.total_reopen_count < REPEATED_REOPEN_MISMATCH_THRESHOLD {
                continue;
            }

            let Some((expected_role, mismatch_signals)) = infer_expected_role_for_task(&task)
            else {
                continue;
            };
            let dispatched_role = dispatched_role_for_task(&task);
            if expected_role == dispatched_role {
                continue;
            }

            let session_count = session_repo.count_for_task(&task.id).await.unwrap_or(0);
            let epic_short_id = if let Some(epic_id) = &task.epic_id {
                sqlx::query_scalar!("SELECT short_id FROM epics WHERE id = ?", epic_id)
                    .fetch_optional(self.db.pool())
                    .await?
                    .unwrap_or_default()
            } else {
                String::new()
            };

            repeated_reopen_role_tool_mismatches.push(serde_json::json!({
                "id": task.id,
                "short_id": task.short_id,
                "title": task.title,
                "status": task.status,
                "issue_type": task.issue_type,
                "dispatched_role": dispatched_role,
                "expected_role": expected_role,
                "total_reopen_count": task.total_reopen_count,
                "session_count": session_count,
                "mismatch_signals": mismatch_signals,
                "reason": role_tool_mismatch_reason(
                    task.total_reopen_count,
                    expected_role,
                    dispatched_role,
                ),
                "epic_short_id": epic_short_id,
            }));
        }

        Ok(serde_json::json!({
            "epic_stats":            epic_stats,
            "stale_tasks":           stale_tasks,
            "review_queue":          review_queue,
            "repeated_reopen_role_tool_mismatches": repeated_reopen_role_tool_mismatches,
            "stale_threshold_hours": stale_hours,
        }))
    }

    /// Heal stale tasks: move `in_progress` tasks older than `stale_hours` back to `open`.
    /// Logs a `status_changed` activity entry for each healed task and emits `TaskUpdated` events.
    pub async fn reconcile(&self, stale_hours: i64) -> Result<serde_json::Value> {
        let events_tx = self.events.clone();
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        // NOTE: dynamic SQL (stale_hours inlined) — compile-time check not possible
        let sql = format!(
            "SELECT id, project_id, short_id, epic_id, title, description, design, issue_type,
                    `status`, priority, owner, labels, acceptance_criteria,
                    reopen_count, continuation_count, verification_failure_count,
                    total_reopen_count, total_verification_failure_count,
                    intervention_count, last_intervention_at,
                    created_at, updated_at, closed_at,
                    close_reason, merge_commit_sha, pr_url, merge_conflict_metadata, memory_refs
             FROM tasks
             WHERE `status` = 'in_progress'
               AND updated_at < DATE_SUB(NOW(3), INTERVAL {stale_hours} HOUR)"
        );
        let stale: Vec<Task> = sqlx::query_as(&sql).fetch_all(&mut *tx).await?;

        let mut healed: Vec<Task> = Vec::new();
        for task in stale {
            sqlx::query!(
                "UPDATE tasks
                 SET `status` = 'open',
                     updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                 WHERE id = ?",
                task.id
            )
            .execute(&mut *tx)
            .await?;

            let activity_id = uuid::Uuid::now_v7().to_string();
            let payload = serde_json::json!({
                "from":   "in_progress",
                "to":     "open",
                "reason": "reconcile_stale",
            })
            .to_string();
            sqlx::query!(
                "INSERT INTO activity_log
                    (id, task_id, actor_id, actor_role, event_type, payload)
                 VALUES (?, ?, 'system', 'system', 'status_changed', ?)",
                activity_id,
                task.id,
                payload
            )
            .execute(&mut *tx)
            .await?;

            let updated: Task = task_select_where_id!(&task.id).fetch_one(&mut *tx).await?;
            healed.push(updated);
        }
        tx.commit().await?;

        for task in &healed {
            events_tx.send(DjinnEventEnvelope::task_updated(task, false));
        }

        let healed_short_ids: Vec<&str> = healed.iter().map(|t| t.short_id.as_str()).collect();
        Ok(serde_json::json!({
            "healed_tasks":      healed.len(),
            "healed_task_ids":   healed_short_ids,
            "recovered_tasks":   0,
            "reviews_triggered": 0,
        }))
    }
}

// ── Dynamic query helpers ─────────────────────────────────────────────────────

/// Build a SQL WHERE clause + params vector from optional filter fields.
///
/// Returns `("1=1", [])` when no filters are supplied.
pub(super) fn build_where(
    project_id: &Option<String>,
    status: &Option<String>,
    issue_type: &Option<String>,
    priority: Option<i64>,
    label: &Option<String>,
    text: &Option<String>,
    parent: &Option<String>,
) -> (String, Vec<SqlParam>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();

    if let Some(p) = project_id {
        clauses.push("project_id = ?".to_owned());
        params.push(SqlParam::Text(p.clone()));
    }

    if let Some(s) = status {
        clauses.push("`status` = ?".to_owned());
        params.push(SqlParam::Text(s.clone()));
    }

    if let Some(it) = issue_type {
        if let Some(neg) = it.strip_prefix('!') {
            clauses.push("issue_type != ?".to_owned());
            params.push(SqlParam::Text(neg.to_owned()));
        } else {
            clauses.push("issue_type = ?".to_owned());
            params.push(SqlParam::Text(it.clone()));
        }
    }

    if let Some(p) = priority {
        clauses.push("priority = ?".to_owned());
        params.push(SqlParam::Integer(p));
    }

    if let Some(lbl) = label {
        clauses.push("JSON_CONTAINS(labels, JSON_QUOTE(?), '$')".to_owned());
        params.push(SqlParam::Text(lbl.clone()));
    }

    if let Some(t) = text {
        clauses.push("(title LIKE ? OR description LIKE ?)".to_owned());
        let pattern = format!("%{t}%");
        params.push(SqlParam::Text(pattern.clone()));
        params.push(SqlParam::Text(pattern));
    }

    if let Some(p) = parent {
        clauses.push("epic_id = ?".to_owned());
        params.push(SqlParam::Text(p.clone()));
    }

    let where_sql = if clauses.is_empty() {
        "1=1".to_owned()
    } else {
        clauses.join(" AND ")
    };

    (where_sql, params)
}

/// Convert a sort key to a SQL ORDER BY clause.
pub(super) fn sort_to_sql(sort: &str) -> &'static str {
    match sort {
        "created" => "created_at ASC",
        "created_desc" => "created_at DESC",
        "updated" => "updated_at ASC",
        "updated_desc" => "updated_at DESC",
        "closed" => "closed_at DESC, created_at DESC",
        _ => "priority ASC, created_at ASC", // default: "priority"
    }
}
